//! probe 主体：拿到 swapchain → 核对 vtable → 装 hook → 每帧打水印。
//!
//! 对应 [77 号 §5.2/§5.6](../../../docs/77-depth-ground-truth-hook-experiment-plan.md) 的 P1a 里程碑。

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufWriter, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, HANDLE, HMODULE, HWND, LPARAM, RECT, WAIT_EVENT,
    WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_DEPTH_STENCIL, D3D11_BIND_SHADER_RESOURCE, D3D11_BOX,
    D3D11_BUFFER_DESC, D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG,
    D3D11_DEPTH_STENCIL_VIEW_DESC, D3D11_DEVICE_CONTEXT_DEFERRED, D3D11_DSV_DIMENSION_TEXTURE2D,
    D3D11_DSV_DIMENSION_TEXTURE2DARRAY, D3D11_DSV_DIMENSION_TEXTURE2DMS,
    D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY, D3D11_KEEP_RENDER_TARGETS_AND_DEPTH_STENCIL,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11_VIEWPORT,
    D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE, D3D11CreateDeviceAndSwapChain,
    ID3D11Buffer, ID3D11CommandList, ID3D11DepthStencilView, ID3D11Device, ID3D11DeviceContext,
    ID3D11DeviceContext1, ID3D11DeviceContext2, ID3D11DeviceContext3, ID3D11DeviceContext4,
    ID3D11Multithread, ID3D11Resource, ID3D11Texture2D, ID3D11View,
};
use windows::Win32::Graphics::Direct3D12::ID3D12Device;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT,
    DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R32_TYPELESS,
    DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_MODE_DESC, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SWAP_CHAIN_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory4, IDXGISwapChain,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, GetCurrentThread, OpenEventW, OpenProcess, PROCESS_SYNCHRONIZE, SetEvent,
    SetThreadInformation, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    THREAD_POWER_THROTTLING_CURRENT_VERSION, THREAD_POWER_THROTTLING_EXECUTION_SPEED,
    THREAD_POWER_THROTTLING_STATE, THREAD_PRIORITY_BELOW_NORMAL, ThreadPowerThrottling,
    WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassExW, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{BOOL, HRESULT, Interface, PCWSTR, w};

use crate::readback::{self, CaptureConfig, RawDepthFrame};
use crate::vtable::{
    D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES, D3D11_DEVICE_CONTEXT1_VTABLE_ENTRIES,
    D3D11_DEVICE_CONTEXT2_VTABLE_ENTRIES, D3D11_DEVICE_CONTEXT3_VTABLE_ENTRIES,
    D3D11_DEVICE_CONTEXT4_VTABLE_ENTRIES, VtableHook, VtableOwnerState, VtableShadow,
    VtableSlotState, d3d11_context, d3d11_multithread, dxgi_swapchain, verify_layout,
};
use crate::writer::DepthWriter;
use depth_lab::decode::ZConvention;

mod present_timing;
use present_timing::{PresentPerfEntry, PresentPerfFrame};

/// 当前帧序号。probe 每次 `Present` 自增，低 16 位即 `present_seq`（§5.6.1）。
static PRESENT_SEQ: AtomicU64 = AtomicU64::new(0);
/// Identity of the adapter behind the actual game swapchain seen by the
/// active generation. Ready is withheld until this has been observed.
static ACTUAL_SWAPCHAIN_ADAPTER: Mutex<Option<depth_lab::GpuAdapterIdentity>> = Mutex::new(None);
/// Lock-free fast-path key for the generation's actual D3D device. A strong
/// reference is retained so a released COM object cannot ABA-reuse the pointer.
/// Multiple swapchains are allowed only when they share this exact device;
/// device recreation is a resource-epoch change and therefore faults v1.
static OBSERVED_SWAPCHAIN_DEVICE_PTR: AtomicUsize = AtomicUsize::new(0);
static OBSERVED_SWAPCHAIN_DEVICE: Mutex<Option<ID3D11Device>> = Mutex::new(None);
static OBSERVED_D3D12_DEVICE: Mutex<Option<ID3D12Device>> = Mutex::new(None);
/// 原 `Present` 函数指针。hook 里必须调它。
static ORIGINAL_PRESENT: AtomicUsize = AtomicUsize::new(0);
/// hook 句柄。持有它是为了 `Drop` 时还原 vtable（否则游戏会崩在我们卸载后的地址上）。
static PRESENT_HOOK: OnceLock<VtableHook> = OnceLock::new();
/// 「**下一帧**要不要采深度」。由 [`update_cadence_for_next_frame`] 在每次
/// `Present` 的末尾写入，由 `OMSetRenderTargets` 与 `capture_last_completed_at_present` 读取。
///
/// # 为什么是一个共享标志而不是两边各算一次
///
/// 这两个判定点必须对**同一帧**给出**同一个结论**：`OMSetRenderTargets` 负责把
/// DSV 快照送进环，`capture_last_completed_at_present` 负责从环里取。
/// 一边采一边不采，取出来的就是别的帧的深度——而且不会报错。
///
/// 旧的 `present_seq % 2` 两边各算各的也一致，因为它是同一个 seq 的纯函数。
/// 改成时间基之后不行了——两者在**不同时刻**读 QPC。
static CAPTURE_THIS_FRAME: AtomicBool = AtomicBool::new(false);
static CADENCE: Mutex<Option<depth_lab::cadence::Cadence>> = Mutex::new(None);
static CADENCE_FIRST_DECISION_LOGGED: AtomicBool = AtomicBool::new(false);
static FORMAL_FIRST_PRESENT_LOGGED: AtomicBool = AtomicBool::new(false);
static SNAPSHOT_FIRST_ELIGIBLE_LOGGED: AtomicBool = AtomicBool::new(false);
/// 目标频率只解析一次——它来自配置文件，每帧读盘不可接受。
static TARGET_DEPTH_HZ: Mutex<Option<f64>> = Mutex::new(None);
/// 车道标称帧率（30/60），来自 StartRun.lane_fps。0 = 未带（旧客户端/手工流程），
/// present 帧率闸停用。闸为什么锚定车道而不是 target_depth_hz：见 #801。
static LANE_FPS: AtomicU32 = AtomicU32::new(0);
/// 逐 Present 的帧率观测。violated 单向置位，StopRun 时落盘成裁决文件。
static PRESENT_RATE_GATE: Mutex<Option<depth_lab::present_rate::PresentRateGate>> =
    Mutex::new(None);
static RATE_VIOLATION_LOGGED: AtomicBool = AtomicBool::new(false);
static LANE_ABSENT_LOGGED: AtomicBool = AtomicBool::new(false);
/// 配置没给目标频率时的兜底（旧格式配置 / 手工 env 流程）。
///
/// 取两个车道里**较低**的那个默认值（lane-30 → 15 Hz）：兜底应当偏保守，
/// 采少了是数据少，采多了是掉帧。真实流程里客户端总会显式写入。
const FALLBACK_TARGET_HZ: f64 = 15.0;

static ORIGINAL_RESIZE_BUFFERS: AtomicUsize = AtomicUsize::new(0);
static RESIZE_BUFFERS_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_SWAPCHAIN_RELEASE: AtomicUsize = AtomicUsize::new(0);
static SWAPCHAIN_RELEASE_HOOK: OnceLock<VtableHook> = OnceLock::new();
/// Retains the dummy D3D12 device and process-wide observation hooks for an
/// early-launched D3D12 process. It is intentionally absent for the existing
/// D3D11 late-injection path.
static D3D12_EARLY_BOOTSTRAP: OnceLock<crate::d3d12_backend::D3d12Bootstrap> = OnceLock::new();
static D3D12_DEPTH_PROFILE: Mutex<Option<(crate::d3d12_backend::D3d12DepthSelector, usize)>> =
    Mutex::new(None);
static D3D12_FORMAL_START_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);

fn compiled_probe_runtime_version() -> &'static str {
    option_env!("DM_DEPTH_PROBE_RUNTIME_VERSION").unwrap_or(env!("CARGO_PKG_VERSION"))
}
static D3D12_PAIRED_WARMUP_FRAMES: AtomicU32 = AtomicU32::new(0);
static D3D12_ACTIVE_SWAPCHAIN: AtomicUsize = AtomicUsize::new(0);
static D3D12_PRESENTATION_LAST_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static D3D12_LAST_ARM_ERROR: Mutex<Option<String>> = Mutex::new(None);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct D3d12DiscoveryConfig {
    schema: String,
    output_dir: String,
    recording_ready: bool,
}

#[derive(Serialize)]
struct D3d12DiscoveryReport {
    schema: &'static str,
    process_launch_id: String,
    recording_ready: bool,
    observation: crate::d3d12_backend::D3d12ObservationSnapshot,
}

fn finalize_d3d12_discovery_snapshot(
    mut snapshot: crate::d3d12_backend::D3d12ObservationSnapshot,
) -> crate::d3d12_backend::D3d12ObservationSnapshot {
    if snapshot.registered_swapchain_count == 0 {
        snapshot
            .blockers
            .push("no factory-observed D3D12 swapchain".into());
    }
    if snapshot.present_calls == 0 {
        snapshot
            .blockers
            .push("no D3D12 Present observation reached discovery".into());
    }
    if snapshot.depth_candidates.is_empty() {
        snapshot
            .blockers
            .push("no D3D12 depth candidate was observed".into());
    }
    snapshot.capture_safe = snapshot.capture_safe && snapshot.blockers.is_empty();
    snapshot
}

static ORIGINAL_MAP: AtomicUsize = AtomicUsize::new(0);
static MAP_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_UNMAP: AtomicUsize = AtomicUsize::new(0);
static UNMAP_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_UPDATE_SUBRESOURCE: AtomicUsize = AtomicUsize::new(0);
static UPDATE_SUBRESOURCE_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_VS_SET_CONSTANT_BUFFERS: AtomicUsize = AtomicUsize::new(0);
static VS_SET_CONSTANT_BUFFERS_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_VS_SET_CONSTANT_BUFFERS1: AtomicUsize = AtomicUsize::new(0);
static VS_SET_CONSTANT_BUFFERS1_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW: AtomicUsize = AtomicUsize::new(0);
static DRAW_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW_INDEXED: AtomicUsize = AtomicUsize::new(0);
static DRAW_INDEXED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW_INDEXED_INSTANCED: AtomicUsize = AtomicUsize::new(0);
static DRAW_INDEXED_INSTANCED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW_INSTANCED: AtomicUsize = AtomicUsize::new(0);
static DRAW_INSTANCED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW_AUTO: AtomicUsize = AtomicUsize::new(0);
static DRAW_AUTO_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW_INDEXED_INSTANCED_INDIRECT: AtomicUsize = AtomicUsize::new(0);
static DRAW_INDEXED_INSTANCED_INDIRECT_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DRAW_INSTANCED_INDIRECT: AtomicUsize = AtomicUsize::new(0);
static DRAW_INSTANCED_INDIRECT_HOOK: OnceLock<VtableHook> = OnceLock::new();
/// 只接受安装 hook 时拿到的游戏 immediate context；共享 vtable 上的 deferred context
/// 调用不属于本采样时间线。
static GAME_IMMEDIATE_CONTEXT: AtomicUsize = AtomicUsize::new(0);
/// D3D11 runtime 的 immediate-context vtable 在 run8 中位于 `context + 8`，Draw 槽会在
/// 每轮 Present 被重铺。Camera 与 DSV-only discovery 都需要完整 shadow 表让对象脱离
/// 这张易变 backing table；逐槽 hook 句柄仍保留 original/replacement 身份与 ABI 转发，
/// 但不再把 backing slot 当存活事实。
static GAME_CONTEXT_VTABLE_SHADOW: OnceLock<VtableShadow> = OnceLock::new();
/// `OnceLock<VtableHook>` 只证明句柄存在，不能证明槽位还指向 replacement。Fallout 4
/// run7 已出现 OM 槽仍命中、七个 Draw 槽入口同时归零；Present 必须独立审计实际
/// immediate context 与槽位，并且只对精确恢复成 original 的槽做安全重挂。
static CAMERA_HOOK_AUDIT_DUE: AtomicBool = AtomicBool::new(true);
static PRODUCTION_VS_QUERY_LOGGED: AtomicBool = AtomicBool::new(false);
static CAMERA_HOOK_INTEGRITY_FAILED: AtomicBool = AtomicBool::new(false);
static CAMERA_HOOK_INTEGRITY_MARKED: AtomicBool = AtomicBool::new(false);
static CAMERA_HOOK_INTEGRITY_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static VIEWPORT_OBSERVATION_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static CAMERA_HOOK_INTEGRITY_AUDIT_LOCK: Mutex<()> = Mutex::new(());
static BACKING_VTABLE_PRESENT_TRANSITION_LOGGED: AtomicBool = AtomicBool::new(false);
static POST_PRESENT_SHADOW_REPAIR_LOGGED: AtomicBool = AtomicBool::new(false);
const CAMERA_HOOK_AUDIT_INTERVAL_PRESENTS: u64 = 60;
/// Keep the render callback non-blocking while absorbing a bounded two-second
/// disk/encoder stall at the certified 30 Hz cadence. A 1280x720 D24 frame is
/// about 3.5 MiB, so the hard upper bound remains below 256 MiB.
const DEPTH_WRITER_QUEUE_CAPACITY: usize = 64;

// `IDXGISwapChain::ResizeBuffers`（vtable 槽 13，见 vtable.rs）的 hook 同样只留了
// 两个 static 而没有安装，故一并删除——`-D warnings` 下 dead_code 即错。
//
// 留一句在这里，因为这不只是「少了个 hook」：分辨率切换会让我们缓存的 D3D 资源
// （RTV、暂存纹理、DSV 描述）全部失效，而失效之后**不会报错**，只会开始产出
// 尺寸对不上的深度。真要支持游戏中途改分辨率，得在 ResizeBuffers 前后清缓存；
// 在那之前，probe 的前提是「一次会话内分辨率不变」——这个前提值得显式写下来。
static ORIGINAL_OM_SET_RENDER_TARGETS: AtomicUsize = AtomicUsize::new(0);
static OM_SET_RENDER_TARGETS_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS: AtomicUsize = AtomicUsize::new(0);
static OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK: OnceLock<VtableHook> =
    OnceLock::new();
static ORIGINAL_CLEAR_DEPTH_STENCIL_VIEW: AtomicUsize = AtomicUsize::new(0);
static CLEAR_DEPTH_STENCIL_VIEW_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_CLEAR_STATE: AtomicUsize = AtomicUsize::new(0);
static CLEAR_STATE_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_SWAP_DEVICE_CONTEXT_STATE: AtomicUsize = AtomicUsize::new(0);
static SWAP_DEVICE_CONTEXT_STATE_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_CLEAR_VIEW: AtomicUsize = AtomicUsize::new(0);
static CLEAR_VIEW_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_SET_MULTITHREAD_PROTECTED: AtomicUsize = AtomicUsize::new(0);
static SET_MULTITHREAD_PROTECTED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static GAME_MULTITHREAD_CONTEXT: OnceLock<ID3D11Multithread> = OnceLock::new();
static ORIGINAL_EXECUTE_COMMAND_LIST: AtomicUsize = AtomicUsize::new(0);
static EXECUTE_COMMAND_LIST_HOOK: OnceLock<VtableHook> = OnceLock::new();
static DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static DEFERRED_DSV_HOOKS_READY: AtomicBool = AtomicBool::new(false);
static DEFERRED_DUMMY_CONTEXT: OnceLock<ID3D11DeviceContext> = OnceLock::new();
static DSV_DISCOVERY_DEVICE: OnceLock<ID3D11Device> = OnceLock::new();
static ORIGINAL_DEFERRED_OM_SET_RENDER_TARGETS: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_OM_SET_RENDER_TARGETS_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS: AtomicUsize =
    AtomicUsize::new(0);
static DEFERRED_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK: OnceLock<VtableHook> =
    OnceLock::new();
static ORIGINAL_DEFERRED_CLEAR_DEPTH_STENCIL_VIEW: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_CLEAR_DEPTH_STENCIL_VIEW_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_CLEAR_STATE: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_CLEAR_STATE_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW_INDEXED: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_INDEXED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW_INDEXED_INSTANCED: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_INDEXED_INSTANCED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW_INSTANCED: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_INSTANCED_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW_AUTO: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_AUTO_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW_INDEXED_INSTANCED_INDIRECT: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_INDEXED_INSTANCED_INDIRECT_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_DRAW_INSTANCED_INDIRECT: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_DRAW_INSTANCED_INDIRECT_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_FINISH_COMMAND_LIST: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_FINISH_COMMAND_LIST_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_EXECUTE_COMMAND_LIST: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_EXECUTE_COMMAND_LIST_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_BEGIN: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_BEGIN_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_END: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_END_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_SWAP_DEVICE_CONTEXT_STATE: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_SWAP_DEVICE_CONTEXT_STATE_HOOK: OnceLock<VtableHook> = OnceLock::new();
static ORIGINAL_DEFERRED_CLEAR_VIEW: AtomicUsize = AtomicUsize::new(0);
static DEFERRED_CLEAR_VIEW_HOOK: OnceLock<VtableHook> = OnceLock::new();
/// Discovery evidence begins only after all game-context hooks have passed their first audit.
/// The bootstrap Present itself is excluded because its OM/draw work occurred before installation.
static DSV_DISCOVERY_OBSERVATION_STARTED: AtomicBool = AtomicBool::new(false);
/// The first full post-install Present interval is a deferred-ledger warmup. It lets every active
/// preexisting context cross a real FinishCommandList boundary without admitting that excluded
/// frame into report counters.
static DSV_DISCOVERY_DEFERRED_WARMUP_STARTED: AtomicBool = AtomicBool::new(false);
/// Reaching the configured retained-frame limit closes hook admission without manufacturing
/// frame-limit drops while the operator later performs the explicit config seal.
static DSV_DISCOVERY_OBSERVATION_COMPLETE: AtomicBool = AtomicBool::new(false);
static DSV_DISCOVERY_FRAME_LIMIT: AtomicU32 = AtomicU32::new(0);
static DSV_DISCOVERY_RETAINED_FRAMES: AtomicU32 = AtomicU32::new(0);
/// 水印矩形原点，来自 profile。P1a 先固定左上角。
static WATERMARK_ORIGIN: (u32, u32) = (0, 0);
/// B0 诊断只在显式启用时运行，避免把 DSV 查询成本带进正常 probe。
static DEPTH_CAPTURE_ENABLED: AtomicBool = AtomicBool::new(false);
/// Hooks and DSV/Camera observation stay live before this flag is armed, but no formal frame,
/// watermark, cadence budget, or Camera timeline may cross the lifecycle Ready boundary early.
static FORMAL_CAPTURE_ARMED: AtomicBool = AtomicBool::new(false);
static UNBIND_FROM_END: AtomicUsize = AtomicUsize::new(4);
static CAPTURE_MAX_FRAMES: AtomicU64 = AtomicU64::new(u64::MAX);
static CAPTURE_ISSUED: AtomicU64 = AtomicU64::new(0);
static CAPTURE_LIMIT_LOGGED: AtomicBool = AtomicBool::new(false);
static DSV_AUDIT_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static DSV_HOOK_INSTALL_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static DSV_AUDIT_SEEN: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
static DSV_HOOK_INSTALL_LOCK: Mutex<()> = Mutex::new(());
/// 当前受审 pass 的 raster draw 数。所有七个 Draw* 入口都汇入这里。
static SELECTED_PASS_DRAW_CALLS: AtomicU64 = AtomicU64::new(0);
/// Diagnostic-only. The normal capture contract never pays for pass identity serialization.
static PASS_IDENTITY_TRACE_ENABLED: AtomicBool = AtomicBool::new(false);
/// Exact-build Camera production captures the DSV pass that contained the configured projection
/// draw. Diagnostic trace runs deliberately keep the full candidate ring so an operator can audit
/// the decision without changing the evidence being inspected.
static EXACT_PRODUCTION_SNAPSHOT_ENABLED: AtomicBool = AtomicBool::new(false);

fn exact_production_snapshot_enabled() -> bool {
    EXACT_PRODUCTION_SNAPSHOT_ENABLED.load(Ordering::Acquire)
}

const fn exact_production_snapshot_mode(camera_production: bool, diagnostic_trace: bool) -> bool {
    camera_production && !diagnostic_trace
}
/// GPU readback 是异步的；pass 证据先按 `present_seq` 暂存，Map 完成后再与原始帧一起入队。
static CAPTURE_PASS_EVIDENCE: Mutex<BTreeMap<u64, PendingCaptureEvidence>> =
    Mutex::new(BTreeMap::new());
/// The viewport used by the exact production projection draw, keyed by the frame it renders.
/// Readback finishes later, so this observation must survive until the depth row is materialized.
static OBSERVED_DRAW_VIEWPORTS: Mutex<BTreeMap<u64, ObservedDrawViewport>> =
    Mutex::new(BTreeMap::new());
static DEPTH_TX: OnceLock<SyncSender<DepthWriterCommand>> = OnceLock::new();
static DEPTH_SEND_LOCK: Mutex<()> = Mutex::new(());
// StopRun must not split one Present callback between depth capture and camera sampling.
// Holding this only around the pre-Present probe work keeps the control barrier short and
// avoids waiting for the game's VSync call.
static PRESENT_RUN_LOCK: Mutex<()> = Mutex::new(());
static ACTIVE_GENERATION: AtomicU64 = AtomicU64::new(0);
static SELECTED_PRESENT_RUNTIME: Mutex<Option<SelectedPresentProbeRuntime>> = Mutex::new(None);
static SELECTED_PRESENT_ENABLED: AtomicBool = AtomicBool::new(false);
static SELECTED_PRESENT_ARMED: AtomicBool = AtomicBool::new(false);
static SELECTED_PRESENT_FATAL: AtomicBool = AtomicBool::new(false);
static SELECTED_PRESENT_RESIZE_FAULT: AtomicBool = AtomicBool::new(false);
static SELECTED_PRESENT_SEALED: AtomicBool = AtomicBool::new(false);
static SELECTED_PRESENT_FAULT: Mutex<
    Option<(depth_lab::selected_present_feedback::FaultReason, String)>,
> = Mutex::new(None);

/// 连续异常已触发中止。只置一次——同一次采集里第二次到线毫无意义，
/// run 那时已经在拆了，而重复置位会把后续帧的日志刷成噪声。
static HEALTH_ABORT_LATCHED: AtomicBool = AtomicBool::new(false);

/// writer 线程 → 控制线程的中止信号（130 号 §6.5）。
///
/// # 为什么要过一道 latch 而不是让 writer 直接停
///
/// 停录要拆的东西（hook、readback、camera production、ACK 序号）全都由控制线程
/// 串行拥有，writer 线程碰它们就是跨线程改 run 状态。既有的
/// [`SELECTED_PRESENT_FAULT`] 与 `CAPTURE_WARMUP_ERROR` 是同一个形状：
/// **出事的线程只留证据，拆运行的那个线程按证据处置。**
static HEALTH_ABORT: Mutex<Option<depth_lab::health_abort::HealthAbort>> = Mutex::new(None);

fn selected_present_accepting_candidates() -> bool {
    SELECTED_PRESENT_ENABLED.load(Ordering::Acquire)
        && SELECTED_PRESENT_ARMED.load(Ordering::Acquire)
        && !SELECTED_PRESENT_FATAL.load(Ordering::Acquire)
        && !SELECTED_PRESENT_SEALED.load(Ordering::Acquire)
}

pub(crate) const fn d3d12_pose_needs_depth_projection(
    copied: bool,
    selected: bool,
    accepting_candidates: bool,
) -> bool {
    // Camera proof copies outside Arm..Seal are released without a depth
    // consumer. Keep their real camera rows, but do not fill the projection
    // cache with entries that no selected-depth drain can ever take.
    copied && (!selected || accepting_candidates)
}

const fn depth_candidate_capture_requested(
    cadence_requested: bool,
    selected_present_enabled: bool,
    selected_present_armed: bool,
    selected_present_sealed: bool,
) -> bool {
    if selected_present_enabled {
        selected_present_armed && !selected_present_sealed
    } else {
        cadence_requested
    }
}

fn depth_candidate_capture_requested_now() -> bool {
    if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire)
        && SELECTED_PRESENT_FATAL.load(Ordering::Acquire)
    {
        return false;
    }
    depth_candidate_capture_requested(
        CAPTURE_THIS_FRAME.load(Ordering::Acquire),
        SELECTED_PRESENT_ENABLED.load(Ordering::Acquire),
        SELECTED_PRESENT_ARMED.load(Ordering::Acquire),
        SELECTED_PRESENT_SEALED.load(Ordering::Acquire),
    )
}
static WRITER_QUEUE_DEPTH: AtomicU64 = AtomicU64::new(0);
/// A/B readback-only 档已完成回读、但在 writer 前主动丢弃的帧数。
static WRITER_BYPASSED: AtomicU64 = AtomicU64::new(0);
static FIRST_PRESENT_SEQ: AtomicU64 = AtomicU64::new(u64::MAX);
static DRAINING: AtomicBool = AtomicBool::new(false);
static CAPTURE_WARMUP: Mutex<Option<CaptureWarmup>> = Mutex::new(None);
static CAPTURE_WARMUP_ERROR: Mutex<Option<String>> = Mutex::new(None);
static LOCAL_COMPAT_FPS: AtomicU32 = AtomicU32::new(0);
static LOCAL_COMPAT_PASSED: AtomicBool = AtomicBool::new(false);
static LOCAL_COMPAT_CHECK: Mutex<
    Option<(
        depth_lab::StreamIdentity,
        depth_lab::compatibility::CadenceCheck,
    )>,
> = Mutex::new(None);

fn configure_local_compatibility(
    identity: &depth_lab::StreamIdentity,
    required_fps: Option<u32>,
) -> Result<(), String> {
    LOCAL_COMPAT_FPS.store(0, Ordering::Release);
    LOCAL_COMPAT_PASSED.store(false, Ordering::Release);
    let mut state = LOCAL_COMPAT_CHECK
        .lock()
        .map_err(|_| "local compatibility lock poisoned")?;
    *state = if let Some(fps) = required_fps {
        let mut frequency = 0i64;
        unsafe { QueryPerformanceFrequency(&mut frequency) }.map_err(|e| e.to_string())?;
        if frequency <= 0 {
            return Err("local compatibility QPC frequency invalid".into());
        }
        Some((
            identity.clone(),
            depth_lab::compatibility::CadenceCheck::new(fps, frequency as u64)?,
        ))
    } else {
        None
    };
    LOCAL_COMPAT_FPS.store(required_fps.unwrap_or(0), Ordering::Release);
    Ok(())
}

fn observe_local_compatibility() {
    if LOCAL_COMPAT_FPS.load(Ordering::Acquire) == 0 {
        return;
    }
    let generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    if generation == 0 {
        return;
    }
    if let Ok(mut state) = LOCAL_COMPAT_CHECK.lock()
        && let Some((identity, gate)) = state.as_mut()
        && identity.generation == generation
    {
        gate.observe(current_qpc());
        LOCAL_COMPAT_PASSED.store(
            gate.last_pass.is_some() && gate.error.is_none(),
            Ordering::Release,
        );
    }
}

fn local_compatibility_error(identity: &depth_lab::StreamIdentity) -> Option<String> {
    if LOCAL_COMPAT_FPS.load(Ordering::Acquire) == 0 {
        return None;
    }
    match LOCAL_COMPAT_CHECK.lock() {
        Ok(state) => match state.as_ref() {
            Some((actual, gate)) if actual == identity => gate.error.clone(),
            _ => Some("DEPTH_LOCAL_VALIDATION_FAILED: 本机校验代次或状态缺失".into()),
        },
        Err(_) => Some("DEPTH_LOCAL_VALIDATION_FAILED: 本机校验状态锁损坏".into()),
    }
}

fn persist_local_compatibility(
    run_identity: &depth_lab::StreamIdentity,
    output_dir: &std::path::Path,
) -> Result<(), String> {
    let required_fps = LOCAL_COMPAT_FPS.load(Ordering::Acquire);
    if required_fps == 0 {
        return Ok(());
    }
    let state = LOCAL_COMPAT_CHECK
        .lock()
        .map_err(|_| "local compatibility lock poisoned")?;
    let (identity, gate) = state.as_ref().ok_or("local compatibility state missing")?;
    let (observed_fps, observed_seconds) = gate
        .last_pass
        .ok_or("local compatibility evidence missing")?;
    if gate.error.is_some() {
        return Err("local compatibility already faulted".into());
    }
    let receipt = depth_lab::compatibility::LocalCompatibilityReceipt {
        schema: "depth-local-compatibility/v1".into(),
        identity: identity.clone(),
        required_fps,
        observed_fps,
        observed_seconds,
    };
    receipt.validate(run_identity, required_fps)?;
    let bytes = serde_json::to_vec_pretty(&receipt).map_err(|e| e.to_string())?;
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output_dir.join(depth_lab::compatibility::RECEIPT_FILE))
        .map_err(|e| e.to_string())?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| e.to_string())
}

static ACTIVE_CONFIG: Mutex<Option<depth_lab::probe_config::ProbeConfig>> = Mutex::new(None);
static CURRENT_EXECUTABLE_SHA256: OnceLock<String> = OnceLock::new();
static CAPTURE_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static DRAIN_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static WRITER_QUEUE_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
/// writer 队列满而被丢弃的帧数。
///
/// **必须单独记。** 这些帧 GPU 回读是成功的，所以已经计进 `ReadbackStats::completed`，
/// 但它们从没落盘。不扣掉就会出现「文件丢了、统计仍显示 100%」的盲区。
static WRITER_QUEUE_DROPPED: AtomicU64 = AtomicU64::new(0);

struct SelectedPresentProbeRuntime {
    transport: depth_lab::selected_present_transport::ProbeTransport,
    stream: depth_lab::selected_present_feedback::StreamEpoch,
    state: depth_lab::selected_present_feedback::FeedbackState,
    ring_state: depth_lab::selected_present_candidate_ring::CandidateRingState,
    gpu_ring: Option<readback::GpuCandidateRing>,
    d3d12_gpu_ring: Option<crate::d3d12_candidate::D3d12CandidateRing>,
    candidate_evidence: BTreeMap<u64, PendingCaptureEvidence>,
    candidate_conv: Option<ZConvention>,
    readback_slots: usize,
    bootstrap: depth_lab::SelectedPresentFeedbackBootstrap,
    pending_arm: Option<
        depth_lab::selected_present_feedback::Envelope<
            depth_lab::selected_present_feedback::Decision,
        >,
    >,
    next_ack_ordinal: u64,
    feedback_latency: FeedbackLatencySamples,
}

#[derive(Debug, Default)]
struct FeedbackLatencySamples {
    qpc_frequency: u64,
    selected_lag_presents: Vec<u64>,
    wgc_delivery_ticks: Vec<u64>,
    wgc_source_ahead_callback_ready_ticks: Vec<u64>,
    callback_to_decision_ticks: Vec<u64>,
    source_to_probe_ticks: Vec<u64>,
    source_lead_probe_ticks: Vec<u64>,
    decision_to_probe_ticks: Vec<u64>,
}

const MAX_FEEDBACK_LATENCY_SAMPLES: usize = 262_144;

impl FeedbackLatencySamples {
    fn new(qpc_frequency: u64) -> Result<Self, String> {
        if qpc_frequency == 0 {
            return Err("QPC frequency is zero for selected-Present latency telemetry".into());
        }
        Ok(Self {
            qpc_frequency,
            ..Self::default()
        })
    }

    fn observe(
        &mut self,
        selected_present_seq: u64,
        current_present_seq: u64,
        qpc: depth_lab::selected_present_feedback::SelectTimestamps,
    ) -> Result<(), (depth_lab::selected_present_feedback::FaultReason, String)> {
        use depth_lab::selected_present_feedback::FaultReason;

        if self.selected_lag_presents.len() >= MAX_FEEDBACK_LATENCY_SAMPLES {
            return Err((
                FaultReason::FeedbackTelemetryOverflow,
                format!(
                    "selected-Present latency sample cap {MAX_FEEDBACK_LATENCY_SAMPLES} exceeded"
                ),
            ));
        }
        let selected_lag = current_present_seq
            .checked_sub(selected_present_seq)
            .ok_or_else(|| {
                (
                    FaultReason::FeedbackTimestampInvalid,
                    "selected Present is newer than the probe callback".into(),
                )
            })?;
        // The contract crate owns what these five readings are allowed to be;
        // this type only accumulates. Each direction arrives pre-split, so a
        // leading compositor clock cannot underflow into a huge fake delay.
        let latency = qpc
            .classify(self.qpc_frequency)
            .map_err(|fault| (fault.reason, fault.to_string()))?;
        self.selected_lag_presents.push(selected_lag);
        self.wgc_delivery_ticks.extend(latency.wgc_delivery_ticks);
        self.wgc_source_ahead_callback_ready_ticks
            .extend(latency.source_lead_callback_ready_ticks);
        self.callback_to_decision_ticks
            .push(latency.callback_to_decision_ticks);
        self.source_to_probe_ticks
            .extend(latency.source_to_probe_ticks);
        self.source_lead_probe_ticks
            .extend(latency.source_lead_probe_ticks);
        self.decision_to_probe_ticks
            .push(latency.decision_to_probe_ticks);
        Ok(())
    }

    fn summary(&self) -> depth_lab::SelectedPresentLatencySummary {
        depth_lab::SelectedPresentLatencySummary {
            sample_count: self.selected_lag_presents.len() as u64,
            selected_lag_presents: quantiles(self.selected_lag_presents.clone()),
            wgc_delivery_sample_count: self.wgc_delivery_ticks.len() as u64,
            wgc_delivery_us: qpc_quantiles_us(self.wgc_delivery_ticks.clone(), self.qpc_frequency),
            wgc_source_ahead_callback_ready_count: self.wgc_source_ahead_callback_ready_ticks.len()
                as u64,
            wgc_source_ahead_callback_ready_us: qpc_quantiles_us(
                self.wgc_source_ahead_callback_ready_ticks.clone(),
                self.qpc_frequency,
            ),
            callback_to_decision_us: qpc_quantiles_us(
                self.callback_to_decision_ticks.clone(),
                self.qpc_frequency,
            ),
            source_to_probe_sample_count: self.source_to_probe_ticks.len() as u64,
            source_to_probe_us: qpc_quantiles_us(
                self.source_to_probe_ticks.clone(),
                self.qpc_frequency,
            ),
            wgc_source_ahead_process_count: self.source_lead_probe_ticks.len() as u64,
            wgc_source_ahead_process_us: qpc_quantiles_us(
                self.source_lead_probe_ticks.clone(),
                self.qpc_frequency,
            ),
            decision_to_probe_us: qpc_quantiles_us(
                self.decision_to_probe_ticks.clone(),
                self.qpc_frequency,
            ),
        }
    }
}

fn quantiles(mut values: Vec<u64>) -> depth_lab::LatencyQuantiles {
    if values.is_empty() {
        return depth_lab::LatencyQuantiles::default();
    }
    values.sort_unstable();
    let at = |numerator: usize, denominator: usize| {
        let rank = values
            .len()
            .saturating_mul(numerator)
            .div_ceil(denominator)
            .saturating_sub(1)
            .min(values.len() - 1);
        values[rank]
    };
    depth_lab::LatencyQuantiles {
        p50: at(50, 100),
        p95: at(95, 100),
        p99: at(99, 100),
        max: *values.last().expect("non-empty latency sample"),
    }
}

fn qpc_quantiles_us(values: Vec<u64>, qpc_frequency: u64) -> depth_lab::LatencyQuantiles {
    let values = values
        .into_iter()
        .map(|ticks| {
            u64::try_from(
                u128::from(ticks).saturating_mul(1_000_000) / u128::from(qpc_frequency.max(1)),
            )
            .unwrap_or(u64::MAX)
        })
        .collect();
    quantiles(values)
}

#[derive(Debug, Clone, Default)]
struct SelectedPresentRuntimeSummary {
    generation: u64,
    resource_epoch: u64,
    first_candidate_present_seq: Option<u64>,
    materialized_candidates: u64,
    selected_frame_count: u64,
    distinct_selected_depth_count: u64,
    source_identity_alias_count: u64,
    readback_submitted_count: u64,
    d3d12_readback_completed: Option<u64>,
    unresolved_candidates: u64,
    max_unresolved_candidates: u32,
    max_feedback_lag_presents: u64,
    feedback_latency: depth_lab::SelectedPresentLatencySummary,
    gpu_allocation: Option<readback::CandidateGpuAllocationStats>,
    gpu_allocation_error: Option<String>,
    feedback_queue_depth: u64,
    ack_queue_depth: u64,
    feedback_queue_max_depth: u64,
    ack_queue_max_depth: u64,
    feedback_queue_full_count: u64,
    ack_queue_full_count: u64,
    transport_fault_reason: u32,
}

fn install_selected_present_runtime(
    runtime: Option<SelectedPresentProbeRuntime>,
) -> Result<(), String> {
    let mut active = SELECTED_PRESENT_RUNTIME
        .lock()
        .map_err(|_| "selected-Present runtime lock poisoned".to_string())?;
    if active.is_some() {
        return Err("previous selected-Present runtime is still active".into());
    }
    SELECTED_PRESENT_SEALED.store(runtime.is_none(), Ordering::Release);
    SELECTED_PRESENT_ENABLED.store(runtime.is_some(), Ordering::Release);
    SELECTED_PRESENT_ARMED.store(false, Ordering::Release);
    SELECTED_PRESENT_FATAL.store(false, Ordering::Release);
    SELECTED_PRESENT_RESIZE_FAULT.store(false, Ordering::Release);
    if let Ok(mut fault) = SELECTED_PRESENT_FAULT.lock() {
        *fault = None;
    }
    *active = runtime;
    Ok(())
}

fn clear_selected_present_runtime() -> Option<SelectedPresentRuntimeSummary> {
    let summary = SELECTED_PRESENT_RUNTIME.lock().ok().and_then(|mut active| {
        active.take().map(|mut runtime| {
            let gpu_allocation_error = runtime
                .gpu_ring
                .as_mut()
                .and_then(|gpu_ring| gpu_ring.refresh_final_stats().err())
                .or_else(|| {
                    runtime
                        .d3d12_gpu_ring
                        .as_mut()
                        .and_then(|ring| ring.refresh_final_stats().err())
                });
            if let Some(error) = &gpu_allocation_error {
                log(&format!(
                    "selected-Present final GPU allocation query failed: {error}"
                ));
            }
            let stats = runtime.ring_state.stats();
            let corrupt = depth_lab::selected_present_transport::QueueStats {
                depth: u64::MAX,
                max_depth: u64::MAX,
                full_count: u64::MAX,
                fatal_reason: depth_lab::selected_present_feedback::FaultReason::TransportCorrupt
                    as u32,
            };
            let decision_queue = runtime.transport.decisions.stats().unwrap_or(corrupt);
            let ack_queue = runtime.transport.acks.stats().unwrap_or(corrupt);
            SelectedPresentRuntimeSummary {
                generation: runtime.stream.generation,
                resource_epoch: runtime.stream.resource_epoch,
                first_candidate_present_seq: runtime.ring_state.first_materialized_present_seq(),
                materialized_candidates: stats.materialized,
                selected_frame_count: stats.selected_messages,
                distinct_selected_depth_count: stats.distinct_selected,
                source_identity_alias_count: stats.source_identity_aliases,
                readback_submitted_count: stats.submitted,
                d3d12_readback_completed: runtime
                    .d3d12_gpu_ring
                    .as_ref()
                    .map(|ring| ring.completed()),
                unresolved_candidates: runtime.ring_state.unresolved() as u64,
                max_unresolved_candidates: stats.max_unresolved,
                max_feedback_lag_presents: stats.max_feedback_lag_presents,
                feedback_latency: runtime.feedback_latency.summary(),
                gpu_allocation: runtime
                    .gpu_ring
                    .as_ref()
                    .map(readback::GpuCandidateRing::stats)
                    .or_else(|| runtime.d3d12_gpu_ring.as_ref().map(|ring| ring.stats())),
                gpu_allocation_error,
                feedback_queue_depth: decision_queue.depth,
                ack_queue_depth: ack_queue.depth,
                feedback_queue_max_depth: decision_queue.max_depth,
                ack_queue_max_depth: ack_queue.max_depth,
                feedback_queue_full_count: decision_queue.full_count,
                ack_queue_full_count: ack_queue.full_count,
                transport_fault_reason: decision_queue.fatal_reason.max(ack_queue.fatal_reason),
            }
        })
    });
    SELECTED_PRESENT_SEALED.store(false, Ordering::Release);
    SELECTED_PRESENT_ENABLED.store(false, Ordering::Release);
    SELECTED_PRESENT_ARMED.store(false, Ordering::Release);
    SELECTED_PRESENT_FATAL.store(false, Ordering::Release);
    SELECTED_PRESENT_RESIZE_FAULT.store(false, Ordering::Release);
    summary
}

fn latch_selected_present_fault(
    reason: depth_lab::selected_present_feedback::FaultReason,
    message: impl Into<String>,
) {
    SELECTED_PRESENT_FATAL.store(true, Ordering::Release);
    if let Ok(mut fault) = SELECTED_PRESENT_FAULT.lock()
        && fault.is_none()
    {
        *fault = Some((reason, message.into()));
    }
}

fn publish_selected_present_fault(
    generation: u64,
    reason: depth_lab::selected_present_feedback::FaultReason,
) {
    // Do not call from latch_selected_present_fault: decision handling can already hold this
    // mutex. The Present/control callers publish before retiring the mapped transport instead.
    let guard = SELECTED_PRESENT_RUNTIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(runtime) = guard.as_ref()
        && runtime.stream.generation == generation
        && let Err(error) = runtime.transport.publish_fault(reason)
    {
        log(&format!(
            "publish selected-Present terminal fault failed: {error}"
        ));
    }
}

/// Called only from `hooked_present` while `PRESENT_RUN_LOCK` is held. It is
/// therefore the sole consumer of the decision queue and sole ACK producer.
fn protocol_fault_reason(
    error: &depth_lab::selected_present_feedback::ProtocolError,
) -> depth_lab::selected_present_feedback::FaultReason {
    match error {
        depth_lab::selected_present_feedback::ProtocolError::OrdinalGap { .. } => {
            depth_lab::selected_present_feedback::FaultReason::DecisionOrdinalGap
        }
        depth_lab::selected_present_feedback::ProtocolError::Fault(reason) => *reason,
        depth_lab::selected_present_feedback::ProtocolError::StreamMismatch("generation") => {
            depth_lab::selected_present_feedback::FaultReason::GenerationMismatch
        }
        depth_lab::selected_present_feedback::ProtocolError::StreamMismatch("resource_epoch") => {
            depth_lab::selected_present_feedback::FaultReason::ResourceEpochMismatch
        }
        depth_lab::selected_present_feedback::ProtocolError::StreamMismatch(_) => {
            depth_lab::selected_present_feedback::FaultReason::IdentityMismatch
        }
        _ => depth_lab::selected_present_feedback::FaultReason::TransportCorrupt,
    }
}

fn apply_candidate_decision(
    runtime: &mut SelectedPresentProbeRuntime,
    decision: &depth_lab::selected_present_feedback::Decision,
    current_present_seq: u64,
    callback_generation: u64,
) -> Result<(), (depth_lab::selected_present_feedback::FaultReason, String)> {
    use depth_lab::selected_present_candidate_ring::SelectPlan;
    use depth_lab::selected_present_feedback::{Decision, FaultReason};

    if callback_generation != runtime.stream.generation {
        return Err((
            FaultReason::GenerationMismatch,
            format!(
                "feedback processed in callback generation {callback_generation}, expected {}",
                runtime.stream.generation
            ),
        ));
    }
    match decision {
        Decision::ArmGeneration => Err((
            FaultReason::ArmBoundaryMissing,
            "ArmGeneration reached candidate application without an actual first candidate".into(),
        )),
        Decision::Select {
            present_seq,
            source_qpc,
            callback_start_qpc,
            callback_ready_qpc,
            decision_qpc,
            ..
        } => {
            // Every CFR Select is a feedback decision, including duplicate aliases. Recording
            // only first-time depth submissions hid stalls behind a healthy-looking distinct
            // sample set and made the feedback latency distribution incomplete.
            runtime.feedback_latency.observe(
                *present_seq,
                current_present_seq,
                depth_lab::selected_present_feedback::SelectTimestamps {
                    source: *source_qpc,
                    callback_start: *callback_start_qpc,
                    callback_ready: *callback_ready_qpc,
                    decision: *decision_qpc,
                    process: current_qpc(),
                },
            )?;
            let plan = runtime
                .ring_state
                .select(
                    runtime.stream.generation,
                    runtime.stream.resource_epoch,
                    *present_seq,
                    current_present_seq,
                )
                .map_err(|fault| (fault.reason, fault.to_string()))?;
            let SelectPlan::Submit(plan) = plan else {
                return Ok(());
            };
            if !runtime.candidate_evidence.contains_key(present_seq) {
                return Err((
                    FaultReason::RingIdentityMismatch,
                    format!("selected present_seq {present_seq} has no candidate evidence"),
                ));
            }
            let accepted = if let Some(ring) = runtime.d3d12_gpu_ring.as_mut() {
                ring.select(plan.slot, *present_seq)
                    .map_err(|error| (FaultReason::ReadbackMapFailed, error))?
            } else {
                let texture = runtime
                    .gpu_ring
                    .as_ref()
                    .ok_or((
                        FaultReason::CandidateAllocationFailed,
                        "selected present arrived before the GPU candidate ring was created".into(),
                    ))?
                    .texture(plan.slot)
                    .map_err(|error| (FaultReason::RingIdentityMismatch, error))?;
                let device = unsafe { texture.GetDevice() }
                    .map_err(|error| (FaultReason::ReadbackMapFailed, error.to_string()))?;
                let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
                    .map_err(|error| (FaultReason::ReadbackMapFailed, error.to_string()))?;
                let cfg = CaptureConfig {
                    dst_w: runtime.bootstrap.width as usize,
                    dst_h: runtime.bootstrap.height as usize,
                    conv: runtime.candidate_conv.ok_or((
                        FaultReason::RingIdentityMismatch,
                        "candidate depth convention is missing".into(),
                    ))?,
                    slots: runtime.readback_slots,
                    deadline_ticks: READBACK_DEADLINE_FRAMES,
                };
                readback::capture_pooled(&device, &context, &texture, *present_seq, cfg)
                    .map_err(|error| (FaultReason::ReadbackMapFailed, error))?
            };
            if !accepted {
                return Err((
                    FaultReason::ReadbackPoolFull,
                    format!("readback pool is full for selected present_seq {present_seq}"),
                ));
            }
            let evidence = runtime
                .candidate_evidence
                .remove(present_seq)
                .expect("candidate evidence was checked before submission");
            if CAPTURE_PASS_EVIDENCE
                .lock()
                .map_err(|_| {
                    (
                        FaultReason::TransportCorrupt,
                        "DSV pass evidence lock poisoned".into(),
                    )
                })?
                .insert(*present_seq, evidence)
                .is_some()
            {
                return Err((
                    FaultReason::RingIdentityMismatch,
                    format!("duplicate submitted evidence for present_seq {present_seq}"),
                ));
            }
            runtime
                .ring_state
                .commit_submitted(plan)
                .map_err(|fault| (fault.reason, fault.to_string()))?;
            CAPTURE_ISSUED.fetch_add(1, Ordering::Relaxed);
            crate::perf::mark_frame(crate::perf::FrameFlag::Capture);
            Ok(())
        }
        Decision::RetireThrough { present_seq } => {
            runtime
                .ring_state
                .retire_through(*present_seq)
                .map_err(|fault| (fault.reason, fault.to_string()))?;
            runtime
                .candidate_evidence
                .retain(|candidate_seq, _| *candidate_seq > *present_seq);
            Ok(())
        }
        Decision::SealGeneration { .. } => {
            runtime
                .ring_state
                .seal()
                .map_err(|fault| (fault.reason, fault.to_string()))?;
            runtime.candidate_evidence.clear();
            if runtime.ring_state.unresolved() != 0 {
                return Err((
                    FaultReason::FlushCountMismatch,
                    "candidate ring is not empty after SealGeneration".into(),
                ));
            }
            Ok(())
        }
    }
}

fn publish_selected_present_ack(
    runtime: &mut SelectedPresentProbeRuntime,
    ack: depth_lab::selected_present_feedback::Ack,
) -> bool {
    let sealed = matches!(
        ack,
        depth_lab::selected_present_feedback::Ack::GenerationSealed { .. }
    );
    let faulted = matches!(ack, depth_lab::selected_present_feedback::Ack::Fault { .. });
    let ack_message = depth_lab::selected_present_feedback::Envelope {
        schema: depth_lab::selected_present_feedback::FEEDBACK_SCHEMA_V3,
        stream: runtime.stream.clone(),
        ordinal: runtime.next_ack_ordinal,
        payload: ack,
    };
    if let Err(error) = runtime.transport.acks.try_push(&ack_message) {
        latch_selected_present_fault(
            depth_lab::selected_present_feedback::FaultReason::AckQueueFull,
            format!("selected-Present ACK enqueue failed: {error}"),
        );
        return false;
    }
    runtime.next_ack_ordinal = runtime.next_ack_ordinal.saturating_add(1);
    if sealed {
        SELECTED_PRESENT_SEALED.store(true, Ordering::Release);
    }
    !faulted
}

fn process_selected_present_feedback(current_present_seq: u64, callback_generation: u64) {
    if SELECTED_PRESENT_FATAL.load(Ordering::Acquire) {
        let reason = SELECTED_PRESENT_FAULT
            .lock()
            .ok()
            .and_then(|fault| fault.as_ref().map(|(reason, _)| *reason));
        if let Some(reason) = reason {
            publish_selected_present_fault(callback_generation, reason);
        }
        return;
    }
    let mut guard = match SELECTED_PRESENT_RUNTIME.lock() {
        Ok(guard) => guard,
        Err(_) => {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::TransportCorrupt,
                "selected-Present runtime lock poisoned",
            );
            return;
        }
    };
    let Some(runtime) = guard.as_mut() else {
        return;
    };
    if let Some(first_candidate_present_seq) = runtime.ring_state.first_materialized_present_seq()
        && let Some(message) = runtime.pending_arm.take()
    {
        let decision_ordinal = message.ordinal;
        let ack = match runtime
            .state
            .apply_arm(message, first_candidate_present_seq)
        {
            Ok(ack) => ack,
            Err(error) => {
                let reason = protocol_fault_reason(&error);
                latch_selected_present_fault(reason, error.to_string());
                depth_lab::selected_present_feedback::Ack::Fault {
                    decision_ordinal,
                    reason,
                }
            }
        };
        if !publish_selected_present_ack(runtime, ack) {
            return;
        }
    }
    if runtime.pending_arm.is_some() {
        return;
    }
    loop {
        let message = match runtime.transport.decisions.try_pop() {
            Ok(Some(message)) => message,
            Ok(None) => break,
            Err(error) => {
                latch_selected_present_fault(
                    depth_lab::selected_present_feedback::FaultReason::TransportCorrupt,
                    format!("selected-Present decision dequeue failed: {error}"),
                );
                break;
            }
        };
        let decision_ordinal = message.ordinal;
        let decision = message.payload.clone();
        if matches!(
            decision,
            depth_lab::selected_present_feedback::Decision::ArmGeneration
        ) {
            let staged = runtime
                .state
                .stage_arm(&message)
                .map_err(|error| (protocol_fault_reason(&error), error.to_string()))
                .and_then(|()| {
                    runtime
                        .ring_state
                        .arm()
                        .map_err(|fault| (fault.reason, fault.to_string()))
                });
            match staged {
                Ok(()) => {
                    runtime.pending_arm = Some(message);
                    SELECTED_PRESENT_ARMED.store(true, Ordering::Release);
                    break;
                }
                Err((reason, error)) => {
                    latch_selected_present_fault(reason, error);
                    let ack = depth_lab::selected_present_feedback::Ack::Fault {
                        decision_ordinal,
                        reason,
                    };
                    let _ = publish_selected_present_ack(runtime, ack);
                    break;
                }
            }
        }
        let ack = match runtime.state.apply(message) {
            Ok(ack) => match apply_candidate_decision(
                runtime,
                &decision,
                current_present_seq,
                callback_generation,
            ) {
                Ok(()) => ack,
                Err((reason, message)) => {
                    latch_selected_present_fault(reason, message);
                    depth_lab::selected_present_feedback::Ack::Fault {
                        decision_ordinal,
                        reason,
                    }
                }
            },
            Err(error) => {
                let reason = protocol_fault_reason(&error);
                latch_selected_present_fault(reason, error.to_string());
                depth_lab::selected_present_feedback::Ack::Fault {
                    decision_ordinal,
                    reason,
                }
            }
        };
        if !publish_selected_present_ack(runtime, ack) {
            break;
        }
    }
}

const CAPTURE_WARMUP_MIN_PRESENTS: u32 = 8;
const CAPTURE_WARMUP_MIN_STABLE_DURATION: Duration = Duration::from_millis(500);
const CAPTURE_WARMUP_MAX_PRESENT_GAP: Duration = Duration::from_millis(125);
// Keep this below the client's default 15 s Start timeout so the caller receives a concrete
// warmup Faulted ACK instead of a generic client-side timeout.
const PERF_CERTIFICATION_SETTLE_SECONDS: u64 = 5;
const PERF_CERTIFICATION_MEASUREMENT_SECONDS: u64 = 300;

#[derive(Debug, Clone)]
struct CaptureWarmupObservation {
    hooks_installed: bool,
    hook_integrity_healthy: bool,
    selected_depth_ready: bool,
    dsv_deliverable: bool,
    exact_draw_viewport_ready: bool,
    camera: crate::camera_probe::WarmupCameraReadiness,
}

impl CaptureWarmupObservation {
    fn is_ready(&self) -> bool {
        self.hooks_installed
            && self.hook_integrity_healthy
            && self.selected_depth_ready
            && self.dsv_deliverable
            && self.exact_draw_viewport_ready
            && self.camera.is_ready()
    }
}

/// 预热期可能的阻塞原因，顺序即位序。渲染线程每个 Present 都要统计一次，
/// 所以判定结果用定长位图表达：自增定长数组，不分配、不拼字符串。
///
/// 这是阻塞判定的**唯一**定义处，`blockers()` 的可读文案由它派生，两者不会漂移。
const WARMUP_BLOCKER_NAMES: [&str; 13] = [
    "game_context_hooks_not_installed",
    "camera_hook_integrity_failed",
    "selected_depth_identity_missing",
    "dsv_audit_not_deliverable",
    "exact_draw_viewport_missing_or_invalid",
    "camera_production_not_ready",
    "exact_draw_view_missing_or_invalid",
    "exact_draw_projection_missing",
    "camera_sampler",
    "player_sampler",
    "swapchain_adapter_not_observed",
    "present_not_visible_or_failed",
    "watermark_not_submitted",
];
const WARMUP_BLOCKER_CAMERA_SAMPLER: usize = 8;
const WARMUP_BLOCKER_PLAYER_SAMPLER: usize = 9;

#[derive(Debug, Clone)]
enum CaptureWarmupEvidence {
    Depth(CaptureWarmupObservation),
    Presentation {
        adapter_observed: bool,
        present_succeeded: bool,
        watermark_required: bool,
        watermark_written: bool,
    },
}

impl From<CaptureWarmupObservation> for CaptureWarmupEvidence {
    fn from(value: CaptureWarmupObservation) -> Self {
        Self::Depth(value)
    }
}

impl CaptureWarmupEvidence {
    fn is_ready(&self) -> bool {
        match self {
            Self::Depth(observation) => observation.is_ready(),
            Self::Presentation {
                adapter_observed,
                present_succeeded,
                watermark_required,
                watermark_written,
            } => {
                *adapter_observed
                    && *present_succeeded
                    && (!watermark_required || *watermark_written)
            }
        }
    }

    /// 无分配的阻塞判定，位序同 [`WARMUP_BLOCKER_NAMES`]。渲染线程每帧调用。
    fn blocker_flags(&self) -> u16 {
        let mut flags = 0_u16;
        let mut set = |index: usize| flags |= 1 << index;
        match self {
            Self::Depth(observation) => {
                if !observation.hooks_installed {
                    set(0);
                }
                if !observation.hook_integrity_healthy {
                    set(1);
                }
                if !observation.selected_depth_ready {
                    set(2);
                }
                if !observation.dsv_deliverable {
                    set(3);
                }
                if !observation.exact_draw_viewport_ready {
                    set(4);
                }
                // 与原判定一致：这三条是 if / else if 链，一次只报最靠前的那个。
                if !observation.camera.production_ready {
                    set(5);
                } else if !observation.camera.view_ready {
                    set(6);
                } else if !observation.camera.projection_ready {
                    set(7);
                }
                if observation.camera.camera_sample_error.is_some() {
                    set(WARMUP_BLOCKER_CAMERA_SAMPLER);
                }
                if observation.camera.player_sample_error.is_some() {
                    set(WARMUP_BLOCKER_PLAYER_SAMPLER);
                }
            }
            Self::Presentation {
                adapter_observed,
                present_succeeded,
                watermark_required,
                watermark_written,
            } => {
                if !adapter_observed {
                    set(10);
                }
                if !present_succeeded {
                    set(11);
                }
                if *watermark_required && !watermark_written {
                    set(12);
                }
            }
        }
        flags
    }

    /// 超时诊断用的可读文案。由 [`Self::blocker_flags`] 派生，采样器那两条额外带上原始错误。
    fn blockers(&self) -> Vec<String> {
        let flags = self.blocker_flags();
        let sampler_detail = |index: usize| match self {
            Self::Depth(observation) => match index {
                WARMUP_BLOCKER_CAMERA_SAMPLER => observation.camera.camera_sample_error.as_deref(),
                WARMUP_BLOCKER_PLAYER_SAMPLER => observation.camera.player_sample_error.as_deref(),
                _ => None,
            },
            Self::Presentation { .. } => None,
        };
        WARMUP_BLOCKER_NAMES
            .iter()
            .enumerate()
            .filter(|(index, _)| flags & (1 << index) != 0)
            .map(|(index, name)| match sampler_detail(index) {
                Some(detail) => format!("{name}={detail}"),
                None => (*name).to_string(),
            })
            .collect()
    }
}

fn camera_warmup_readiness(
    generation: u64,
    present_seq: u64,
) -> crate::camera_probe::WarmupCameraReadiness {
    if crate::kcd2_live_pose::enabled() {
        let ready = crate::kcd2_live_pose::pair_ready_for_present(present_seq);
        return crate::camera_probe::WarmupCameraReadiness {
            production_ready: true,
            view_ready: ready,
            projection_ready: ready,
            camera_sample_error: crate::kcd2_live_pose::failure()
                .or_else(crate::kcd2_live_pose::projection_warmup_error),
            player_sample_error: None,
        };
    }
    crate::camera_probe::warmup_present_readiness(generation, present_seq)
}

fn submit_camera_present(generation: u64, present_seq: u64, render_evidence: bool) -> bool {
    if crate::kcd2_live_pose::enabled() {
        return crate::kcd2_live_pose::pair_ready_for_present(present_seq);
    }
    crate::camera_probe::on_present(generation, present_seq, render_evidence)
}

fn camera_runtime_failure() -> Option<String> {
    if crate::kcd2_live_pose::enabled() {
        crate::kcd2_live_pose::failure()
    } else {
        crate::camera_probe::production_failure()
    }
}

fn begin_stop_camera_production() {
    if crate::kcd2_live_pose::enabled() {
        crate::kcd2_live_pose::stop();
    } else {
        crate::camera_probe::begin_stop_production();
    }
}

fn finish_stop_camera_production() -> crate::camera_probe::CameraProductionSummary {
    if !crate::kcd2_live_pose::enabled() {
        return crate::camera_probe::finish_stop_production();
    }
    crate::kcd2_live_pose::stop();
    let pose = crate::kcd2_live_pose::summary();
    let mut errors = Vec::new();
    if pose.faults != 0 || pose.queue_drops != 0 {
        errors.push(pose.error.unwrap_or_else(|| {
            format!(
                "KCD2 live pose stopped with faults={} queue_drops={}",
                pose.faults, pose.queue_drops
            )
        }));
    }
    crate::camera_probe::CameraProductionSummary {
        pose_row_count: pose.pairs,
        player_pose_row_count: Some(pose.pairs),
        diagnostic_row_count: pose.discards,
        last_present_seq: pose.last_pair_present,
        errors,
    }
}

fn camera_last_submitted_present_seq() -> Option<u64> {
    if d3d12_presentation_only() {
        let seq = D3D12_PRESENTATION_LAST_PRESENT.load(Ordering::Acquire);
        return (seq != u64::MAX).then_some(seq);
    }
    if crate::kcd2_live_pose::enabled() {
        crate::kcd2_live_pose::summary().last_pair_present
    } else {
        crate::camera_probe::last_submitted_present_seq()
    }
}

fn begin_camera_production_warmup(generation: u64) -> Result<(), String> {
    if crate::kcd2_live_pose::enabled() {
        crate::kcd2_live_pose::begin_production_warmup()
    } else {
        crate::camera_probe::begin_production_warmup(generation)
    }
}

fn arm_camera_production(generation: u64) -> Result<(), String> {
    if crate::kcd2_live_pose::enabled() {
        crate::kcd2_live_pose::arm_production()
    } else {
        crate::camera_probe::arm_production(generation)
    }
}

#[derive(Debug)]
struct CaptureWarmup {
    generation: u64,
    observed_presents: u64,
    qualified_presents: u64,
    consecutive_qualified_presents: u32,
    max_consecutive_qualified_presents: u32,
    continuity_resets: u32,
    stable_since: Option<Instant>,
    last_qualified_at: Option<Instant>,
    last_observation: Option<CaptureWarmupEvidence>,
    // 以下仅供诊断汇总，不参与任何准入判定。
    began_at: Instant,
    first_observed_at: Option<Instant>,
    first_qualified_at: Option<Instant>,
    ready_at: Option<Instant>,
    /// 每条阻塞原因各卡住了多少个 Present，位序同 [`WARMUP_BLOCKER_NAMES`]。
    blocker_presents: [u32; WARMUP_BLOCKER_NAMES.len()],
}

impl CaptureWarmup {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            observed_presents: 0,
            qualified_presents: 0,
            consecutive_qualified_presents: 0,
            max_consecutive_qualified_presents: 0,
            continuity_resets: 0,
            stable_since: None,
            last_qualified_at: None,
            last_observation: None,
            began_at: Instant::now(),
            first_observed_at: None,
            first_qualified_at: None,
            ready_at: None,
            blocker_presents: [0; WARMUP_BLOCKER_NAMES.len()],
        }
    }

    fn reset_continuity(&mut self) {
        if self.consecutive_qualified_presents > 0 {
            self.continuity_resets = self.continuity_resets.saturating_add(1);
        }
        self.consecutive_qualified_presents = 0;
        self.stable_since = None;
        self.last_qualified_at = None;
    }

    fn observe(
        &mut self,
        generation: u64,
        now: Instant,
        observation: impl Into<CaptureWarmupEvidence>,
    ) -> bool {
        if generation == 0 || generation != self.generation {
            return false;
        }
        self.observed_presents = self.observed_presents.saturating_add(1);
        self.first_observed_at.get_or_insert(now);
        let observation = observation.into();
        let qualified = observation.is_ready();
        if !qualified {
            // 定长数组自增，无分配：这条路径是渲染线程每个 Present 都要走的。
            let flags = observation.blocker_flags();
            for (index, count) in self.blocker_presents.iter_mut().enumerate() {
                if flags & (1 << index) != 0 {
                    *count = count.saturating_add(1);
                }
            }
        }
        self.last_observation = Some(observation);

        // Readiness is a stream of deliverable render Presents, not every raw swapchain Present.
        // Fallout can Present much faster than the 15 Hz depth lane. Non-deliverable Presents in
        // between two qualified samples must not erase the warmup; only a real qualified-sample
        // gap beyond the continuity budget breaks the sequence.
        if !qualified {
            if self
                .last_qualified_at
                .and_then(|last| now.checked_duration_since(last))
                .is_some_and(|gap| gap > CAPTURE_WARMUP_MAX_PRESENT_GAP)
            {
                self.reset_continuity();
            }
            return false;
        }

        let gap_is_stable = self
            .last_qualified_at
            .and_then(|last| now.checked_duration_since(last))
            .is_none_or(|gap| gap <= CAPTURE_WARMUP_MAX_PRESENT_GAP);
        if !gap_is_stable {
            self.reset_continuity();
        }
        self.last_qualified_at = Some(now);
        self.first_qualified_at.get_or_insert(now);
        let stable_since = *self.stable_since.get_or_insert(now);
        self.qualified_presents = self.qualified_presents.saturating_add(1);
        self.consecutive_qualified_presents = self.consecutive_qualified_presents.saturating_add(1);
        self.max_consecutive_qualified_presents = self
            .max_consecutive_qualified_presents
            .max(self.consecutive_qualified_presents);
        let ready = self.consecutive_qualified_presents >= CAPTURE_WARMUP_MIN_PRESENTS
            && now.duration_since(stable_since) >= CAPTURE_WARMUP_MIN_STABLE_DURATION;
        if ready {
            self.ready_at.get_or_insert(now);
        }
        ready
    }

    /// 预热成功时的结构化摘要。**只在预热结束调用一次**，不在每个 Present 的路径上。
    ///
    /// 超时失败已有 `diagnostic_summary`；成功的那一轮此前什么都不留，于是「预热为什么要
    /// 花几秒」在真机上无从对账。`blocker_presents` 回答的正是这个：每条判据各卡住了多少
    /// 个 Present，一眼看出是哪一条在拖，还是整体都在等第一帧。
    fn completion_summary_json(&self) -> String {
        let since_begin = |at: Option<Instant>| {
            at.and_then(|at| at.checked_duration_since(self.began_at))
                .map(|elapsed| elapsed.as_secs_f64() * 1_000.0)
        };
        let blockers = self
            .blocker_presents
            .iter()
            .enumerate()
            .filter(|(_, count)| **count != 0)
            .map(|(index, count)| (WARMUP_BLOCKER_NAMES[index], *count))
            .collect::<std::collections::BTreeMap<_, _>>();
        serde_json::json!({
            "generation": self.generation,
            "observed_presents": self.observed_presents,
            "qualified_presents": self.qualified_presents,
            "max_consecutive_qualified_presents": self.max_consecutive_qualified_presents,
            "continuity_resets": self.continuity_resets,
            "first_present_ms": since_begin(self.first_observed_at),
            "first_qualified_ms": since_begin(self.first_qualified_at),
            "ready_ms": since_begin(self.ready_at),
            "blocker_presents": blockers,
        })
        .to_string()
    }

    fn diagnostic_summary(&self) -> String {
        let blockers = self
            .last_observation
            .as_ref()
            .map(CaptureWarmupEvidence::blockers)
            .unwrap_or_else(|| vec!["no_present_observed".into()]);
        format!(
            "observed_presents={} qualified_presents={} current_streak={} max_streak={} continuity_resets={} last_blockers=[{}]",
            self.observed_presents,
            self.qualified_presents,
            self.consecutive_qualified_presents,
            self.max_consecutive_qualified_presents,
            self.continuity_resets,
            blockers.join(", ")
        )
    }
}

fn capture_warmup_timed_out(started_at: Instant, now: Instant, timeout: Duration) -> bool {
    now.checked_duration_since(started_at)
        .is_some_and(|elapsed| elapsed >= timeout)
}

type DrawFn = unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32);
type DrawIndexedFn = unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32, i32);
type DrawIndexedInstancedFn =
    unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32, u32, i32, u32);
type DrawInstancedFn = unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32, u32, u32);
type DrawAutoFn = unsafe extern "system" fn(*mut core::ffi::c_void);
type DrawIndirectFn =
    unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u32);
type OmSetRenderTargetsFn = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    u32,
    *const *mut core::ffi::c_void,
    *mut core::ffi::c_void,
);
type OmSetRenderTargetsAndUnorderedAccessViewsFn = unsafe extern "system" fn(
    *mut core::ffi::c_void,
    u32,
    *const *mut core::ffi::c_void,
    *mut core::ffi::c_void,
    u32,
    u32,
    *const *mut core::ffi::c_void,
    *const u32,
);
type ExecuteCommandListFn =
    unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, BOOL);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum CameraDrawMethod {
    DrawIndexed = 0,
    Draw = 1,
    DrawIndexedInstanced = 2,
    DrawInstanced = 3,
    DrawAuto = 4,
    DrawIndexedInstancedIndirect = 5,
    DrawInstancedIndirect = 6,
}

impl CameraDrawMethod {
    pub(crate) const ALL: [Self; 7] = [
        Self::DrawIndexed,
        Self::Draw,
        Self::DrawIndexedInstanced,
        Self::DrawInstanced,
        Self::DrawAuto,
        Self::DrawIndexedInstancedIndirect,
        Self::DrawInstancedIndirect,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::DrawIndexed => "draw_indexed",
            Self::Draw => "draw",
            Self::DrawIndexedInstanced => "draw_indexed_instanced",
            Self::DrawInstanced => "draw_instanced",
            Self::DrawAuto => "draw_auto",
            Self::DrawIndexedInstancedIndirect => "draw_indexed_instanced_indirect",
            Self::DrawInstancedIndirect => "draw_instanced_indirect",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
enum OmBindMethod {
    SetRenderTargets = 0,
    SetRenderTargetsAndUnorderedAccessViews = 1,
}

impl OmBindMethod {
    const ALL: [Self; 2] = [
        Self::SetRenderTargets,
        Self::SetRenderTargetsAndUnorderedAccessViews,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::SetRenderTargets => "om_set_render_targets",
            Self::SetRenderTargetsAndUnorderedAccessViews => {
                "om_set_render_targets_and_unordered_access_views"
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OmDepthBindingAction {
    Keep,
    Set(*mut core::ffi::c_void),
}

struct CameraHookEntryCounters {
    draws: [AtomicU64; 7],
    om_binds: [AtomicU64; 2],
    foreign_draws: [AtomicU64; 7],
    foreign_om_binds: [AtomicU64; 2],
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct CameraHookEntrySnapshot {
    draws: [u64; 7],
    om_binds: [u64; 2],
    foreign_draws: [u64; 7],
    foreign_om_binds: [u64; 2],
}

impl CameraHookEntryCounters {
    const fn new() -> Self {
        Self {
            draws: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            om_binds: [AtomicU64::new(0), AtomicU64::new(0)],
            foreign_draws: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            foreign_om_binds: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    fn record_draw(&self, method: CameraDrawMethod) -> u64 {
        self.draws[method as usize].fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record_om_bind(&self, method: OmBindMethod) -> u64 {
        self.om_binds[method as usize].fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record_foreign_draw(&self, method: CameraDrawMethod) -> u64 {
        self.foreign_draws[method as usize].fetch_add(1, Ordering::Relaxed) + 1
    }

    fn record_foreign_om_bind(&self, method: OmBindMethod) -> u64 {
        self.foreign_om_binds[method as usize].fetch_add(1, Ordering::Relaxed) + 1
    }

    fn take(&self) -> CameraHookEntrySnapshot {
        let mut snapshot = CameraHookEntrySnapshot::default();
        for method in CameraDrawMethod::ALL {
            snapshot.draws[method as usize] = self.draws[method as usize].swap(0, Ordering::AcqRel);
        }
        for method in OmBindMethod::ALL {
            snapshot.om_binds[method as usize] =
                self.om_binds[method as usize].swap(0, Ordering::AcqRel);
            snapshot.foreign_om_binds[method as usize] =
                self.foreign_om_binds[method as usize].swap(0, Ordering::AcqRel);
        }
        for method in CameraDrawMethod::ALL {
            snapshot.foreign_draws[method as usize] =
                self.foreign_draws[method as usize].swap(0, Ordering::AcqRel);
        }
        snapshot
    }
}

static CAMERA_HOOK_ENTRY_COUNTERS: CameraHookEntryCounters = CameraHookEntryCounters::new();

const MAIN_DSV_ID: &str = "main-scene-v1";
const DELIVERY_WIDTH: usize = 1280;
const DELIVERY_HEIGHT: usize = 720;
const DEFAULT_READBACK_SLOTS: usize = depth_lab::readback::ReadbackPool::MIN_SLOTS;
const READBACK_DEADLINE_FRAMES: u64 = 4;

#[derive(Clone)]
struct SelectedDepth {
    // Keep both interfaces alive. A resource pointer alone cannot distinguish two DSVs over the
    // same texture, and releasing the view makes pointer reuse/ABA impossible to audit.
    view: ID3D11DepthStencilView,
    texture: ID3D11Texture2D,
    descriptor: depth_lab::DsvDiscoveryDescriptor,
    identity: DsvSelectionIdentity,
}

impl SelectedDepth {
    fn pair(&self) -> DsvPairIdentity {
        DsvPairIdentity {
            resource_tag: self.texture.as_raw() as usize as u64,
            view_tag: self.view.as_raw() as usize as u64,
        }
    }
}

// D3D11 immediate-context resources are used only from the game's render hook. The mutex prevents
// concurrent hook callers from replacing the retained COM reference while an unbind capture clones it.
unsafe impl Send for SelectedDepth {}

struct DepthTexture(ID3D11Texture2D);

unsafe impl Send for DepthTexture {}

static CURRENT_SELECTED_DEPTH: Mutex<Option<SelectedDepth>> = Mutex::new(None);
/// Keeps a removed exact view/resource owner alive until the same pair is rebound or a serial
/// replacement is installed. The FSM's raw pointer ledger is not an ABA defense by itself.
static RETAINED_UNBOUND_SELECTED_DEPTH: Mutex<Option<SelectedDepth>> = Mutex::new(None);
/// [`CURRENT_SELECTED_DEPTH`] 的原子镜像：0 = 未选中，非 0 = 选中纹理的资源指针值。
/// 每个 Mutex 写点在持锁区间内同步更新它；Draw 热路径只做非零判定、绝不解引用。
/// 渲染线程串行保证镜像与真值之间没有可观察的窗口。
static CURRENT_SELECTED_RAW: AtomicUsize = AtomicUsize::new(0);
/// Phase 热切发生在 DSV 暂时未绑定的窗口时，保留历史唯一 view/resource pair，但绝不
/// 据此直接放行 Draw。它只用于 phase-reset strict reacquire；普通 pass 的 null-unbind
/// 绝不能占用这组 latch，而应由 [`DepthSelectionState::pending_explicit_unbind`] 裁决串行
/// 换代。0 表示没有待重获 pair。
static DISCOVERY_DSV_REACQUIRE_RESOURCE: AtomicUsize = AtomicUsize::new(0);
static DISCOVERY_DSV_REACQUIRE_VIEW: AtomicUsize = AtomicUsize::new(0);
static DISCOVERY_DSV_REACQUIRE_LAST_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
static DISCOVERY_DSV_REACQUIRE_ERROR_LOGGED: AtomicBool = AtomicBool::new(false);
static DISCOVERY_DSV_REACQUIRE_SUCCESS_LOGGED: AtomicBool = AtomicBool::new(false);
/// 非证据帧只记一次“OM 绑定可能已变”，不查询 DSV COM 对象。下一证据帧首个 Draw
/// 再从真实 context 精确重获，避免把跨 Present 的旧引用当成本帧主场景 pass。
static DSV_BINDING_DIRTY: AtomicBool = AtomicBool::new(true);
static DSV_REACQUIRE_LAST_PRESENT: AtomicU64 = AtomicU64::new(u64::MAX);
/// True only for production profiles with an exact DSV format pin and evidence path.
/// Runtime ambiguity and MSAA remain fail-closed.
static PROFILE_DSV_OPERATOR_EXACT_PIN: AtomicBool = AtomicBool::new(false);
/// True only while camera-discovery v3 owns an exact replayed descriptor. During the stop/start
/// phase gap there is deliberately no discovery callback epoch; render hooks must mark the OM
/// state dirty and defer to strict phase reacquire instead of mutating the old/new phase ledger.
static CAMERA_DISCOVERY_V3_EXACT_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Serializes the old discovery epoch's stop/quiesce with the new epoch's Start ACK. A render
/// callback that first sees no owner waits here and retries callback admission, so no OM mutation
/// can fall into an unaccounted phase gap.
static CAMERA_DISCOVERY_PHASE_TRANSITION_LOCK: Mutex<()> = Mutex::new(());

fn enter_owned_discovery_callback() -> Option<crate::camera_probe::DiscoveryCallbackGuard> {
    let first = crate::camera_probe::enter_discovery_callback();
    if first.is_some() || !CAMERA_DISCOVERY_V3_EXACT_ACTIVE.load(Ordering::Acquire) {
        return first;
    }
    let transition = match CAMERA_DISCOVERY_PHASE_TRANSITION_LOCK.lock() {
        Ok(transition) => transition,
        Err(poisoned) => {
            drop(poisoned.into_inner());
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            log(
                "camera discovery phase transition lock poisoned; callback admission failed closed",
            );
            return None;
        }
    };
    let retried = crate::camera_probe::enter_discovery_callback();
    drop(transition);
    retried
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DepthSelectionExpectation {
    LegacyRender(depth_lab::CameraDiscoveryRenderIdentity),
    ExactDescriptor(depth_lab::DsvDiscoveryDescriptor),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DsvPairIdentity {
    resource_tag: u64,
    view_tag: u64,
}

/// Legacy profiles identify the texture, while v3 discovery attests an exact DSV/resource pair.
/// Keep the actual COM pair separately: view aliases must not weaken v3 ownership or ABA checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DsvSelectionIdentity {
    Resource(u64),
    ExactPair(DsvPairIdentity),
}

impl DsvSelectionIdentity {
    fn matches_pair(self, pair: DsvPairIdentity) -> bool {
        match self {
            Self::Resource(resource) => resource == pair.resource_tag,
            Self::ExactPair(expected) => expected == pair,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetainedUnboundTransition {
    Keep,
    RetainOutgoing,
    Release,
}

fn retained_unbound_transition(
    outgoing_pair: Option<DsvPairIdentity>,
    incoming_pair: Option<DsvPairIdentity>,
) -> RetainedUnboundTransition {
    match (outgoing_pair, incoming_pair) {
        (Some(_), None) => RetainedUnboundTransition::RetainOutgoing,
        (Some(outgoing), Some(incoming)) if outgoing != incoming => {
            RetainedUnboundTransition::RetainOutgoing
        }
        (_, Some(_)) => RetainedUnboundTransition::Release,
        (None, None) => RetainedUnboundTransition::Keep,
    }
}

const MAX_RETIRED_DSV_PAIRS: usize = 64;

#[derive(Default)]
struct DepthSelectionState {
    expected: Option<DepthSelectionExpectation>,
    selected_pair: Option<DsvPairIdentity>,
    retired_pairs: BTreeSet<DsvSelectionIdentity>,
    pending_explicit_unbind: Option<DsvPairIdentity>,
    ambiguous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseDepthRecovery {
    AwaitInitialSelection,
    PreserveCurrent { pair: DsvPairIdentity },
    ReacquireOnDraw { pair: DsvPairIdentity },
}

impl PhaseDepthRecovery {
    fn selected_resource(self) -> Option<u64> {
        match self {
            Self::AwaitInitialSelection => None,
            Self::PreserveCurrent { pair } | Self::ReacquireOnDraw { pair } => {
                Some(pair.resource_tag)
            }
        }
    }

    fn current_resource(self) -> Option<u64> {
        match self {
            Self::PreserveCurrent { pair } => Some(pair.resource_tag),
            Self::AwaitInitialSelection | Self::ReacquireOnDraw { .. } => None,
        }
    }

    fn strategy_name(self) -> &'static str {
        match self {
            Self::AwaitInitialSelection => "await_initial_omset",
            Self::PreserveCurrent { .. } => "preserve_current",
            Self::ReacquireOnDraw { .. } => "draw_omget_reacquire",
        }
    }
}

impl DepthSelectionState {
    fn identity(&self, pair: DsvPairIdentity) -> DsvSelectionIdentity {
        if matches!(
            self.expected,
            Some(DepthSelectionExpectation::LegacyRender(_))
        ) {
            DsvSelectionIdentity::Resource(pair.resource_tag)
        } else {
            DsvSelectionIdentity::ExactPair(pair)
        }
    }

    fn has_unique_resource(&self) -> bool {
        self.selected_pair.is_some() && !self.ambiguous
    }

    fn selected_resource(&self) -> Option<u64> {
        self.selected_pair.map(|pair| pair.resource_tag)
    }

    fn viewport(&self) -> Option<depth_lab::Viewport> {
        match self.expected.as_ref()? {
            DepthSelectionExpectation::LegacyRender(render) => Some(depth_lab::Viewport {
                width: render.width,
                height: render.height,
            }),
            DepthSelectionExpectation::ExactDescriptor(descriptor) => Some(depth_lab::Viewport {
                width: descriptor.width,
                height: descriptor.height,
            }),
        }
    }

    fn note_explicit_unbind(&mut self, pair: DsvPairIdentity) {
        if self
            .selected_pair
            .is_some_and(|selected| self.identity(selected).matches_pair(pair))
        {
            self.pending_explicit_unbind = self.selected_pair;
        }
    }

    fn accept_matching_pair(&mut self, pair: DsvPairIdentity) -> Result<(), String> {
        if self.ambiguous {
            return Err("受审 DSV identity 已命中歧义 view/resource；本 run 拒绝继续选择".into());
        }
        if self.retired_pairs.contains(&self.identity(pair)) {
            self.ambiguous = true;
            return Err(format!(
                "已退休 DSV view/resource pair 重新出现（ABA）: resource={:#x} view={:#x}",
                pair.resource_tag, pair.view_tag
            ));
        }
        if let Some(previous) = self.selected_pair
            && !self.identity(previous).matches_pair(pair)
        {
            // A different identity requires an explicit unbind. Retire the whole texture for
            // legacy profiles so another view cannot bypass ABA; v3 still retires exact pairs.
            if self.pending_explicit_unbind == Some(previous) {
                if self.retired_pairs.len() >= MAX_RETIRED_DSV_PAIRS {
                    self.ambiguous = true;
                    return Err(format!(
                        "retired DSV pair ledger exceeded bounded capacity {MAX_RETIRED_DSV_PAIRS}"
                    ));
                }
                self.retired_pairs.insert(self.identity(previous));
                self.selected_pair = Some(pair);
                self.pending_explicit_unbind = None;
                return Ok(());
            }
            self.ambiguous = true;
            return Err(format!(
                "受审 DSV descriptor 同时命中多个 view/resource pair: first=({:#x},{:#x}) second=({:#x},{:#x})",
                previous.resource_tag, previous.view_tag, pair.resource_tag, pair.view_tag
            ));
        }
        self.selected_pair = Some(pair);
        self.pending_explicit_unbind = None;
        Ok(())
    }
}

/// Permanently close an exact-descriptor run after a runtime integrity failure. The return value
/// tells the caller whether it must also clear the retained COM source and Draw fast-path mirror.
fn latch_exact_depth_selection_ambiguity(
    discovery_guard: Option<&crate::camera_probe::DiscoveryCallbackGuard>,
) -> bool {
    let mark_discovery = match DEPTH_SELECTION.lock() {
        Ok(mut selection) => match latch_exact_selection_ambiguity_state(&mut selection) {
            Some(mark) => mark,
            None => return false,
        },
        // A poisoned selection ledger cannot authorize further capture. Conservatively close the
        // fast path even if the exact expectation can no longer be inspected.
        Err(_) => true,
    };
    if mark_discovery && let Some(guard) = discovery_guard {
        crate::camera_probe::mark_discovery_dsv_ambiguous(guard);
    }
    true
}

/// `Some` means the selection uses exact v3 authority; its boolean is true only for the first
/// transition into ambiguity. `None` keeps legacy selection behavior unchanged.
fn latch_exact_selection_ambiguity_state(selection: &mut DepthSelectionState) -> Option<bool> {
    if !matches!(
        selection.expected.as_ref(),
        Some(DepthSelectionExpectation::ExactDescriptor(_))
    ) {
        return None;
    }
    let mark = !selection.ambiguous;
    selection.ambiguous = true;
    Some(mark)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectedDepthLockFailure {
    WouldBlock,
    Poisoned,
}

const fn selected_depth_lock_failure_is_terminal(failure: SelectedDepthLockFailure) -> bool {
    matches!(failure, SelectedDepthLockFailure::Poisoned)
}

fn latch_exact_after_lock_failure(
    failure: SelectedDepthLockFailure,
    discovery_guard: Option<&crate::camera_probe::DiscoveryCallbackGuard>,
) -> bool {
    selected_depth_lock_failure_is_terminal(failure)
        && latch_exact_depth_selection_ambiguity(discovery_guard)
}

fn update_retained_unbound_selected_depth(
    outgoing: Option<SelectedDepth>,
    incoming_pair: Option<DsvPairIdentity>,
    discovery_guard: Option<&crate::camera_probe::DiscoveryCallbackGuard>,
) -> Result<(), String> {
    if !CAMERA_DISCOVERY_V3_EXACT_ACTIVE.load(Ordering::Acquire) {
        return Ok(());
    }
    let transition =
        retained_unbound_transition(outgoing.as_ref().map(SelectedDepth::pair), incoming_pair);
    if transition == RetainedUnboundTransition::Keep {
        return Ok(());
    }
    let mut retained = match RETAINED_UNBOUND_SELECTED_DEPTH.lock() {
        Ok(retained) => retained,
        Err(poisoned) => {
            drop(poisoned.into_inner());
            latch_exact_after_lock_failure(SelectedDepthLockFailure::Poisoned, discovery_guard);
            return Err("retained unbound DSV owner lock poisoned".into());
        }
    };
    match transition {
        RetainedUnboundTransition::Keep => unreachable!(),
        RetainedUnboundTransition::RetainOutgoing => {
            *retained = Some(outgoing.ok_or("missing outgoing DSV owner for retention")?);
        }
        RetainedUnboundTransition::Release => *retained = None,
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ObservedDsvResource {
    resource_tag: u64,
    view_tag: u64,
    descriptor: depth_lab::DsvDiscoveryDescriptor,
}

/// Hard bounds for the discovery-only deferred-command ledger. These values cover a 600-Present
/// inventory window with the command-list rate observed during AC3 bring-up while keeping retained
/// COM references and event storage below a predictable process-local ceiling.
const MAX_DSV_DEFERRED_CONTEXTS: usize = 64;
const MAX_DSV_DEFERRED_COMMAND_LISTS: usize = 16_384;
const MAX_DSV_DEFERRED_EVENTS_PER_RECORDING: usize = 16_384;
const MAX_DSV_DEFERRED_EVENTS_TOTAL: usize = 1_048_576;
const DEFERRED_HOOK_COVERAGE_FAILURE: &str = "deferred context hook coverage failed";

#[derive(Clone)]
struct DeferredDsvSnapshot {
    observed: ObservedDsvResource,
    // A list can be executed after the game releases its DSV. Retain the view so the run-local
    // pointer tags cannot undergo allocator ABA while the command list remains replayable.
    _owner: ID3D11DepthStencilView,
}

#[derive(Clone)]
enum DeferredDsvEvent {
    Bind(Option<DeferredDsvSnapshot>),
    Clear(DeferredDsvSnapshot),
    Draw,
}

struct DeferredContextRecording {
    // Retaining the context avoids object-pointer ABA. The process-wide hook is on the runtime's
    // shared deferred-context vtable; it does not depend on this particular interface pointer.
    _owner: ID3D11DeviceContext,
    /// Exact implementation table proven when this actual context first entered the ledger.
    /// A later per-object/partial table must not inherit that proof merely because the COM pointer
    /// is still present in `contexts`.
    verified_vtable_address: usize,
    events: Vec<DeferredDsvEvent>,
    active: Option<DeferredDsvSnapshot>,
    state_known: bool,
    generation_complete: bool,
}

struct DeferredCommandListRecording {
    // Retaining each command list makes its raw pointer a stable run-local lookup key and permits
    // sound repeated ExecuteCommandList replay.
    _owner: ID3D11CommandList,
    events: Arc<[DeferredDsvEvent]>,
    complete: bool,
}

#[derive(Default)]
struct DeferredDsvLedger {
    contexts: BTreeMap<usize, DeferredContextRecording>,
    command_lists: BTreeMap<usize, DeferredCommandListRecording>,
    retained_events: usize,
}

static DEFERRED_DSV_LEDGER: OnceLock<Mutex<DeferredDsvLedger>> = OnceLock::new();

fn deferred_dsv_ledger() -> &'static Mutex<DeferredDsvLedger> {
    DEFERRED_DSV_LEDGER.get_or_init(|| Mutex::new(DeferredDsvLedger::default()))
}

fn deferred_context_belongs_to_discovery_device(
    context: *mut core::ffi::c_void,
) -> Result<bool, String> {
    let expected = DSV_DISCOVERY_DEVICE
        .get()
        .ok_or("D3D11 discovery device identity is missing")?;
    let borrowed = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("deferred DSV callback context is null")?;
    let actual = unsafe { borrowed.GetDevice() }
        .map_err(|error| format!("deferred context GetDevice failed: {error}"))?;
    Ok(actual.as_raw() == expected.as_raw())
}

impl DeferredDsvLedger {
    fn complete_command_list_events(
        &self,
        command_list: *mut core::ffi::c_void,
    ) -> Option<Arc<[DeferredDsvEvent]>> {
        self.command_lists
            .get(&(command_list as usize))
            .filter(|source| source.complete)
            .map(|source| source.events.clone())
    }

    fn ensure_context(&mut self, context: *mut core::ffi::c_void) -> Result<(), String> {
        let key = context as usize;
        if self.contexts.contains_key(&key) {
            return Ok(());
        }
        if context.is_null() || self.contexts.len() >= MAX_DSV_DEFERRED_CONTEXTS {
            return Err("deferred DSV context ledger limit/identity failure".into());
        }
        let borrowed = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
            .ok_or("deferred DSV callback context is null")?;
        if unsafe { borrowed.GetType() } != D3D11_DEVICE_CONTEXT_DEFERRED {
            return Err("deferred DSV hook was entered by a non-deferred context".into());
        }
        if DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire) {
            verify_deferred_context_hook_coverage(borrowed)
                .map_err(|error| format!("{DEFERRED_HOOK_COVERAGE_FAILURE}: {error}"))?;
        }
        self.contexts.insert(
            key,
            DeferredContextRecording {
                _owner: borrowed.clone(),
                verified_vtable_address: context_vtable_address(context),
                events: Vec::new(),
                active: None,
                // A context first encountered after injection may already be in the middle of a
                // list. Its first Finish is therefore a boundary, not complete evidence.
                state_known: false,
                generation_complete: false,
            },
        );
        Ok(())
    }

    fn revalidate_context_hook_coverage(
        &self,
        context: *mut core::ffi::c_void,
    ) -> Result<(), String> {
        let recording = self
            .contexts
            .get(&(context as usize))
            .ok_or("deferred DSV context disappeared before hook coverage revalidation")?;
        let actual_vtable_address = context_vtable_address(context);
        if actual_vtable_address == 0 || actual_vtable_address != recording.verified_vtable_address
        {
            return Err(format!(
                "deferred context vtable owner changed after admission: expected={:#x} actual={actual_vtable_address:#x}",
                recording.verified_vtable_address
            ));
        }
        let borrowed = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
            .ok_or("deferred DSV context is null during hook coverage revalidation")?;
        if unsafe { borrowed.GetType() } != D3D11_DEVICE_CONTEXT_DEFERRED {
            return Err("deferred DSV context type changed before FinishCommandList".into());
        }
        if DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire) {
            verify_deferred_context_hook_coverage(borrowed)?;
        }
        Ok(())
    }

    fn push_event(
        &mut self,
        context: *mut core::ffi::c_void,
        event: DeferredDsvEvent,
    ) -> Result<(), String> {
        self.ensure_context(context)?;
        let key = context as usize;
        let recording = self
            .contexts
            .get_mut(&key)
            .ok_or("deferred DSV context disappeared during event recording")?;

        apply_deferred_event_state(recording, &event);

        if recording.events.len() >= MAX_DSV_DEFERRED_EVENTS_PER_RECORDING
            || self.retained_events >= MAX_DSV_DEFERRED_EVENTS_TOTAL
        {
            recording.generation_complete = false;
            return Err("deferred DSV event ledger limit reached".into());
        }
        recording.events.push(event);
        self.retained_events += 1;
        Ok(())
    }

    fn taint_context(
        &mut self,
        context: *mut core::ffi::c_void,
        reset_to_default: bool,
    ) -> Result<(), String> {
        self.ensure_context(context)?;
        let recording = self
            .contexts
            .get_mut(&(context as usize))
            .ok_or("deferred DSV context disappeared while being tainted")?;
        recording.generation_complete = false;
        if reset_to_default {
            recording.active = None;
            recording.state_known = true;
        }
        Ok(())
    }

    #[cfg(test)]
    fn merge_command_list(
        &mut self,
        context: *mut core::ffi::c_void,
        command_list: *mut core::ffi::c_void,
        restore_context_state: bool,
    ) -> Result<bool, String> {
        self.ensure_context(context)?;
        let source = self.complete_command_list_events(command_list);
        let Some(source) = source else {
            self.taint_context(context, !restore_context_state)?;
            return Ok(false);
        };
        let prepared = prepare_nested_dsv_events(&source);
        self.merge_prepared_events(context, prepared, restore_context_state)?;
        Ok(true)
    }

    fn merge_prepared_events(
        &mut self,
        context: *mut core::ffi::c_void,
        prepared: Vec<DeferredDsvEvent>,
        restore_context_state: bool,
    ) -> Result<(), String> {
        self.ensure_context(context)?;
        let (entry_state_known, entry_active) = self
            .contexts
            .get(&(context as usize))
            .map(|recording| (recording.state_known, recording.active.clone()))
            .ok_or("deferred DSV context disappeared during nested list merge")?;
        let restore_event = if restore_context_state && entry_state_known {
            Some(DeferredDsvEvent::Bind(entry_active.clone()))
        } else if restore_context_state {
            None
        } else {
            Some(DeferredDsvEvent::Bind(None))
        };
        let additional = prepared.len() + usize::from(restore_event.is_some());
        let recording = self
            .contexts
            .get_mut(&(context as usize))
            .ok_or("deferred DSV context disappeared before nested list append")?;
        if recording.events.len().saturating_add(additional) > MAX_DSV_DEFERRED_EVENTS_PER_RECORDING
            || self.retained_events.saturating_add(additional) > MAX_DSV_DEFERRED_EVENTS_TOTAL
        {
            recording.generation_complete = false;
            return Err("deferred DSV nested event ledger limit reached".into());
        }
        for event in prepared {
            apply_deferred_event_state(recording, &event);
            recording.events.push(event);
        }
        if let Some(event) = restore_event {
            apply_deferred_event_state(recording, &event);
            recording.events.push(event);
        } else {
            recording.active = entry_active;
            recording.state_known = false;
            recording.generation_complete = false;
        }
        self.retained_events += additional;
        Ok(())
    }

    fn seal_command_list(
        &mut self,
        context: *mut core::ffi::c_void,
        command_list: ID3D11CommandList,
        restore_deferred_context_state: bool,
    ) -> Result<bool, String> {
        self.ensure_context(context)?;
        let context_key = context as usize;
        // Revalidate the actual context immediately before this list can become replayable. The
        // dummy-vtable Present audit cannot see a later per-object/partial vtable, and accepting a
        // list after (for example) only its OM slot escaped would replay draws against stale DSV
        // state. Preserve the real Finish boundary below, but force this list opaque and return an
        // error so the admitted caller records a sticky VtableOrSlotDrift reason.
        let coverage_error = self.revalidate_context_hook_coverage(context).err();
        if coverage_error.is_some()
            && let Some(recording) = self.contexts.get_mut(&context_key)
        {
            recording.generation_complete = false;
        }
        let mut recording = self
            .contexts
            .remove(&context_key)
            .ok_or("deferred DSV context disappeared during FinishCommandList")?;
        let events = std::mem::take(&mut recording.events);
        let complete = recording.generation_complete;
        let event_count = events.len();
        let command_list_key = command_list.as_raw() as usize;

        let stored = if self.command_lists.contains_key(&command_list_key)
            || self.command_lists.len() >= MAX_DSV_DEFERRED_COMMAND_LISTS
        {
            self.retained_events = self.retained_events.saturating_sub(event_count);
            false
        } else {
            self.command_lists.insert(
                command_list_key,
                DeferredCommandListRecording {
                    _owner: command_list,
                    events: if complete {
                        Arc::from(events)
                    } else {
                        Arc::from([])
                    },
                    complete,
                },
            );
            if !complete {
                self.retained_events = self.retained_events.saturating_sub(event_count);
            }
            true
        };

        if restore_deferred_context_state {
            // Finish(TRUE) preserves the deferred context state for the next recording. Add an
            // explicit synthetic initial bind so replay never depends on invisible inherited state.
            recording.generation_complete = recording.state_known;
            if let Some(active) = recording.active.clone() {
                if recording.events.len() < MAX_DSV_DEFERRED_EVENTS_PER_RECORDING
                    && self.retained_events < MAX_DSV_DEFERRED_EVENTS_TOTAL
                {
                    recording.events.push(DeferredDsvEvent::Bind(Some(active)));
                    self.retained_events += 1;
                } else {
                    recording.generation_complete = false;
                }
            }
        } else {
            recording.active = None;
            recording.state_known = true;
            recording.generation_complete = true;
        }
        self.contexts.insert(context_key, recording);

        if let Some(error) = coverage_error {
            return Err(format!("{DEFERRED_HOOK_COVERAGE_FAILURE}: {error}"));
        }
        if !stored {
            return Err("deferred DSV command-list ledger identity/limit failure".into());
        }
        Ok(complete)
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn apply_deferred_event_state(recording: &mut DeferredContextRecording, event: &DeferredDsvEvent) {
    match event {
        DeferredDsvEvent::Bind(value) => {
            recording.active = value.clone();
            recording.state_known = true;
        }
        DeferredDsvEvent::Clear(_) => {}
        DeferredDsvEvent::Draw if !recording.state_known => {
            recording.generation_complete = false;
        }
        DeferredDsvEvent::Draw => {}
    }
}

fn prepare_nested_dsv_events(source: &[DeferredDsvEvent]) -> Vec<DeferredDsvEvent> {
    let mut prepared = Vec::with_capacity(source.len().saturating_add(1));
    // ExecuteCommandList clears destination state before executing the source list.
    prepared.push(DeferredDsvEvent::Bind(None));
    prepared.extend(source.iter().cloned());
    prepared
}

fn inspect_deferred_dsv(
    depth_stencil_view: *mut core::ffi::c_void,
) -> Result<DeferredDsvSnapshot, String> {
    let owner = unsafe { ID3D11DepthStencilView::from_raw_borrowed(&depth_stencil_view) }
        .ok_or("deferred DSV pointer is null")?
        .clone();
    Ok(DeferredDsvSnapshot {
        observed: inspect_dsv_resource(depth_stencil_view)?,
        _owner: owner,
    })
}

fn record_deferred_ledger_failure(
    entry: &crate::dsv_discovery::ObservationEntryGuard,
    error: &str,
) {
    let reason = if error.starts_with(DEFERRED_HOOK_COVERAGE_FAILURE) {
        crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift
    } else {
        crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure
    };
    entry.record_integrity_failure(reason);
}

fn record_deferred_dsv_event(context: *mut core::ffi::c_void, event: DeferredDsvEvent) {
    if !deferred_dsv_ledger_capture_active() {
        return;
    }
    let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
        return;
    };
    record_deferred_dsv_event_admitted(context, event, &entry);
}

fn record_deferred_dsv_event_admitted(
    context: *mut core::ffi::c_void,
    event: DeferredDsvEvent,
    entry: &crate::dsv_discovery::ObservationEntryGuard,
) {
    if !DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire) {
        entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
        );
        return;
    }
    match deferred_context_belongs_to_discovery_device(context) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
            );
            if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                log(&format!(
                    "D3D11 deferred context identity failed closed: {error}"
                ));
            }
            return;
        }
    }
    match deferred_dsv_ledger().lock() {
        Ok(mut ledger) => {
            if !deferred_dsv_ledger_capture_active() {
                return;
            }
            if let Err(error) = ledger.push_event(context, event) {
                record_deferred_ledger_failure(entry, &error);
                if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                    log(&format!(
                        "D3D11 deferred DSV event ledger failed closed: {error}"
                    ));
                }
            }
        }
        Err(_) => entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
        ),
    }
}

fn record_deferred_dsv_bind(
    context: *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
) {
    if !deferred_dsv_ledger_capture_active() {
        return;
    }
    let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
        return;
    };
    match deferred_context_belongs_to_discovery_device(context) {
        Ok(true) => {}
        Ok(false) => return,
        Err(error) => {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
            );
            if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                log(&format!(
                    "D3D11 deferred bind context identity failed closed: {error}"
                ));
            }
            return;
        }
    }
    let value = if depth_stencil_view.is_null() {
        None
    } else {
        match inspect_deferred_dsv(depth_stencil_view) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                entry.record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                );
                if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                    log(&format!(
                        "D3D11 deferred DSV inspection failed closed: {error}"
                    ));
                }
                return;
            }
        }
    };
    record_deferred_dsv_event_admitted(context, DeferredDsvEvent::Bind(value), &entry);
}

fn taint_deferred_dsv_generation(context: *mut core::ffi::c_void, reset_to_default: bool) {
    if !deferred_dsv_ledger_capture_active() {
        return;
    }
    let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
        return;
    };
    match deferred_context_belongs_to_discovery_device(context) {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
            );
            return;
        }
    }
    match deferred_dsv_ledger().lock() {
        Ok(mut ledger) => {
            if !deferred_dsv_ledger_capture_active() {
                return;
            }
            if let Err(error) = ledger.taint_context(context, reset_to_default) {
                record_deferred_ledger_failure(&entry, &error);
            }
        }
        Err(_) => entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
        ),
    }
}

fn reset_deferred_dsv_ledger() -> Result<(), String> {
    let mut ledger = deferred_dsv_ledger()
        .lock()
        .map_err(|_| "deferred DSV ledger lock poisoned during reset".to_string())?;
    ledger.reset();
    Ok(())
}

#[derive(Debug, Clone)]
struct TrackedDsvResource {
    stable_id: String,
    resource_tag: u64,
    view_tag: u64,
    descriptor: depth_lab::DsvDiscoveryDescriptor,
}

#[derive(Debug, Clone, Default)]
struct DsvFrameCounts {
    draw_calls: u64,
    clears: u32,
    binds: u32,
    unbinds: u32,
    passes: u32,
    first_bound_order: Option<u64>,
    last_bound_order: Option<u64>,
    carried_from_previous_present: bool,
    completed_passes: Vec<depth_lab::DsvDiscoveryCompletedPassObservation>,
    dropped_completed_passes: u32,
}

#[derive(Debug, Clone, Copy)]
struct ActiveDsvPass {
    tracking_tag: u64,
    draw_calls: u64,
    clear_count: u32,
    carried_from_previous_present: bool,
}

/// DSV ranking 的每帧账本。正式路径沿用 resource key；discovery 用 view key，避免把
/// 同一 resource 上的两个 DSV 合并。两种指针都只留在进程内，报告只写首次观察顺序
/// 生成的 `dsv-0001` ordinal，绝不把 ASLR 地址当成跨 run 身份。
#[derive(Default)]
struct DsvFrameTracker {
    resources: BTreeMap<u64, TrackedDsvResource>,
    frame: BTreeMap<u64, DsvFrameCounts>,
    active_resource: Option<u64>,
    bind_order: u64,
    next_id: u64,
    active_carried: bool,
    active_discovery_pass: Option<ActiveDsvPass>,
    discovery_mode: bool,
}

impl DsvFrameTracker {
    fn tracking_tag(&self, observed: ObservedDsvResource) -> u64 {
        if self.discovery_mode {
            observed.view_tag
        } else {
            observed.resource_tag
        }
    }

    fn register(&mut self, observed: ObservedDsvResource) -> bool {
        let tracking_tag = self.tracking_tag(observed);
        if let Some(existing) = self.resources.get(&tracking_tag) {
            // Formal capture has always keyed/ranked by the Texture2D resource. A second DSV view
            // of that resource must not become a production identity failure. Discovery instead
            // keys by the DSV view and audits both its exact descriptor and underlying resource.
            if self.discovery_mode
                && (existing.resource_tag != observed.resource_tag
                    || existing.descriptor != observed.descriptor)
            {
                crate::dsv_discovery::record_descriptor_identity_conflict();
                return false;
            }
            return true;
        }
        let discovery_limit = crate::dsv_discovery::max_candidates();
        if discovery_limit != 0 && self.resources.len() >= discovery_limit {
            crate::dsv_discovery::record_candidate_limit();
            return false;
        }
        if !self.resources.contains_key(&tracking_tag) {
            self.next_id += 1;
            self.resources.insert(
                tracking_tag,
                TrackedDsvResource {
                    stable_id: format!("dsv-{:04}", self.next_id),
                    resource_tag: observed.resource_tag,
                    view_tag: observed.view_tag,
                    descriptor: observed.descriptor,
                },
            );
        }
        true
    }

    /// 把 Draw 热路径攒下的免锁计数归账给**当前** active DSV。
    ///
    /// 必须在每个改变归属或消费账本的持锁入口（bind/clear/finish/reset）**开头**调用：
    /// pending 里的 draw 全部发生在上一次持锁入口之后、本次之前，渲染线程串行保证
    /// 这段时间 active_resource 未变，归属与逐 draw 持锁计数完全一致。
    fn settle_pending_draws(&mut self) {
        self.settle_pending(DSV_PENDING_DRAWS.swap(0, Ordering::Relaxed));
    }

    fn settle_pending(&mut self, pending: u32) {
        if pending != 0
            && let Some(resource_tag) = self.active_resource
        {
            let counts = self.frame.entry(resource_tag).or_default();
            if self.active_carried && counts.passes == 0 {
                counts.carried_from_previous_present = true;
                counts.passes = 1;
            }
            counts.draw_calls = counts.draw_calls.saturating_add(u64::from(pending));
            if self.discovery_mode
                && let Some(active) = self.active_discovery_pass.as_mut()
            {
                if active.tracking_tag == resource_tag {
                    active.draw_calls = active.draw_calls.saturating_add(u64::from(pending));
                } else {
                    crate::dsv_discovery::record_integrity_failure(
                        crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                    );
                }
            }
        }
    }

    fn finish_discovery_pass(&mut self, tracking_tag: u64) {
        if !self.discovery_mode {
            return;
        }
        let Some(pass) = self.active_discovery_pass.take() else {
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
            );
            return;
        };
        if pass.tracking_tag != tracking_tag {
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
            );
            return;
        }
        let counts = self.frame.entry(tracking_tag).or_default();
        if counts.completed_passes.len()
            < depth_lab::MAX_DSV_DISCOVERY_COMPLETED_PASSES_PER_CANDIDATE_PER_FRAME
        {
            counts
                .completed_passes
                .push(depth_lab::DsvDiscoveryCompletedPassObservation(
                    pass.draw_calls,
                    pass.clear_count,
                    pass.carried_from_previous_present,
                ));
        } else {
            counts.dropped_completed_passes = counts.dropped_completed_passes.saturating_add(1);
            crate::dsv_discovery::record_pass_ledger_drop();
        }
    }

    fn bind(&mut self, observed: Option<ObservedDsvResource>) {
        self.settle_pending_draws();
        let previous = self.active_resource;
        let incoming = observed.and_then(|resource| self.register(resource).then_some(resource));
        let incoming_tag = incoming.map(|resource| self.tracking_tag(resource));
        if previous != incoming_tag
            && let Some(previous) = previous
        {
            let counts = self.frame.entry(previous).or_default();
            if self.active_carried && counts.passes == 0 {
                counts.carried_from_previous_present = true;
                counts.passes = 1;
            }
            counts.unbinds = counts.unbinds.saturating_add(1);
            self.finish_discovery_pass(previous);
        }
        self.active_resource = incoming.map(|resource| {
            let tracking_tag = self.tracking_tag(resource);
            self.bind_order += 1;
            let counts = self.frame.entry(tracking_tag).or_default();
            if previous == Some(tracking_tag) && self.active_carried && counts.passes == 0 {
                counts.carried_from_previous_present = true;
                counts.passes = 1;
            } else if previous != Some(tracking_tag) {
                counts.passes = counts.passes.saturating_add(1);
            }
            counts.binds = counts.binds.saturating_add(1);
            counts.first_bound_order.get_or_insert(self.bind_order);
            counts.last_bound_order = Some(self.bind_order);
            self.active_carried = false;
            if self.discovery_mode && previous != Some(tracking_tag) {
                self.active_discovery_pass = Some(ActiveDsvPass {
                    tracking_tag,
                    draw_calls: 0,
                    clear_count: 0,
                    carried_from_previous_present: false,
                });
            }
            tracking_tag
        });
        if self.active_resource.is_none() {
            self.active_carried = false;
            self.active_discovery_pass = None;
        }
        DSV_ACTIVE_TAG.store(self.active_resource.unwrap_or(0), Ordering::Relaxed);
    }

    fn clear(&mut self, observed: ObservedDsvResource) {
        self.settle_pending_draws();
        if !self.register(observed) {
            return;
        }
        let tracking_tag = self.tracking_tag(observed);
        let counts = self.frame.entry(tracking_tag).or_default();
        if self.active_resource == Some(tracking_tag) && self.active_carried && counts.passes == 0 {
            counts.carried_from_previous_present = true;
            counts.passes = 1;
        }
        counts.clears = counts.clears.saturating_add(1);
        if self.discovery_mode
            && self.active_resource == Some(tracking_tag)
            && let Some(active) = self.active_discovery_pass.as_mut()
        {
            if active.tracking_tag == tracking_tag {
                active.clear_count = active.clear_count.saturating_add(1);
            } else {
                crate::dsv_discovery::record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                );
            }
        }
    }

    fn take_frame_counts(
        &mut self,
        selected_resource: Option<u64>,
    ) -> (BTreeMap<u64, DsvFrameCounts>, Option<String>) {
        self.settle_pending_draws();
        let retained = selected_resource.or(self.active_resource);
        if let Some(resource) = retained
            && self.resources.contains_key(&resource)
        {
            let counts = self.frame.entry(resource).or_default();
            if self.active_resource == Some(resource) && self.active_carried && counts.passes == 0 {
                counts.carried_from_previous_present = true;
                counts.passes = 1;
            }
        }
        let counts = std::mem::take(&mut self.frame);
        self.bind_order = 0;
        self.active_carried = self.active_resource.is_some();
        if let Some(active) = self.active_discovery_pass.as_mut() {
            active.carried_from_previous_present = true;
        }
        let selected_id = selected_resource
            .and_then(|resource| self.resources.get(&resource))
            .map(|resource| resource.stable_id.clone());
        (counts, selected_id)
    }

    fn finish_frame(
        &mut self,
        selected_resource: Option<u64>,
    ) -> (Vec<depth_lab::DsvObservation>, Option<String>) {
        let (counts, selected_id) = self.take_frame_counts(selected_resource);
        let observations = counts
            .into_iter()
            .filter_map(|(resource_tag, counts)| {
                let resource = self.resources.get(&resource_tag)?;
                Some(depth_lab::DsvObservation {
                    id: resource.stable_id.clone(),
                    width: resource.descriptor.width,
                    height: resource.descriptor.height,
                    sample_count: resource.descriptor.sample_count,
                    draw_calls: counts.draw_calls,
                    clears: counts.clears,
                    last_bound_order: counts.last_bound_order.unwrap_or(0),
                })
            })
            .collect();
        (observations, selected_id)
    }

    fn finish_discovery_frame(
        &mut self,
        present_seq: u64,
    ) -> depth_lab::DsvDiscoveryFrameObservation {
        let (counts, _) = self.take_frame_counts(None);
        let candidates = counts
            .into_iter()
            .filter_map(|(resource_tag, counts)| {
                let resource = self.resources.get(&resource_tag)?;
                Some(depth_lab::DsvDiscoveryCandidateObservation {
                    candidate_id: resource.stable_id.clone(),
                    descriptor: resource.descriptor,
                    bind_count: counts.binds,
                    unbind_count: counts.unbinds,
                    pass_count: counts.passes,
                    draw_calls: counts.draw_calls,
                    clear_count: counts.clears,
                    completed_passes: counts.completed_passes,
                    dropped_completed_passes: counts.dropped_completed_passes,
                    first_bound_order: counts.first_bound_order,
                    last_bound_order: counts.last_bound_order,
                    carried_from_previous_present: counts.carried_from_previous_present,
                })
            })
            .collect();
        depth_lab::DsvDiscoveryFrameObservation {
            present_seq,
            candidates,
            ambiguous_descriptor_groups: Vec::new(),
        }
    }

    fn active_observed(&self) -> Option<ObservedDsvResource> {
        let tracking_tag = self.active_resource?;
        let tracked = self.resources.get(&tracking_tag)?;
        Some(ObservedDsvResource {
            resource_tag: tracked.resource_tag,
            view_tag: tracked.view_tag,
            descriptor: tracked.descriptor,
        })
    }

    /// Replay the exact OM/Clear/Draw order captured while a deferred context recorded one command
    /// list. D3D11 clears immediate-context state around execution; RestoreContextState only
    /// restores the entry state afterwards. Those implicit transitions are represented through the
    /// same pass FSM so a list cannot leak its final DSV into later immediate draws.
    fn replay_deferred(&mut self, events: &[DeferredDsvEvent], restore_context_state: bool) {
        self.settle_pending_draws();
        let restore = restore_context_state
            .then(|| self.active_observed())
            .flatten();
        self.bind(None);
        for event in events {
            match event {
                DeferredDsvEvent::Bind(snapshot) => {
                    self.bind(snapshot.as_ref().map(|snapshot| snapshot.observed));
                }
                DeferredDsvEvent::Clear(snapshot) => self.clear(snapshot.observed),
                DeferredDsvEvent::Draw => self.settle_pending(1),
            }
        }
        self.bind(restore);
    }

    fn reset(&mut self) {
        *self = Self::default();
        // generation 边界：丢弃未冲账的计数，镜像回到「无绑定」。
        DSV_PENDING_DRAWS.store(0, Ordering::Relaxed);
        DSV_ACTIVE_TAG.store(0, Ordering::Relaxed);
    }

    fn reset_for_discovery(&mut self) {
        self.reset();
        self.discovery_mode = true;
    }
}

static DSV_FRAME_TRACKER: OnceLock<Mutex<DsvFrameTracker>> = OnceLock::new();
/// [`DsvFrameTracker::active_resource`] 的原子镜像：0 = 无绑定。写点全部持有 tracker 锁
/// 且都在渲染线程上；Draw 热路径免锁读它决定要不要计数，不解引用指针值。
static DSV_ACTIVE_TAG: AtomicU64 = AtomicU64::new(0);
/// Draw 热路径的免锁 draw 计数，由下一次持锁入口（bind/clear/finish/reset）冲账进
/// [`DsvFrameTracker::frame`]。D3D11 immediate context 单线程保证 Draw 与 OM/Clear/Present
/// 回调严格串行，因此冲账归属与逐 draw 持锁计数逐位一致，且不再有 try_lock 失败模式。
static DSV_PENDING_DRAWS: AtomicU32 = AtomicU32::new(0);
static DSV_RANK_MISMATCH_LOGGED: AtomicBool = AtomicBool::new(false);
static DSV_RANK_AMBIGUOUS_LOGGED: AtomicBool = AtomicBool::new(false);
static DSV_RANK_MSAA_LOGGED: AtomicBool = AtomicBool::new(false);

fn dsv_frame_tracker() -> &'static Mutex<DsvFrameTracker> {
    DSV_FRAME_TRACKER.get_or_init(|| Mutex::new(DsvFrameTracker::default()))
}

static DEPTH_SELECTION: Mutex<DepthSelectionState> = Mutex::new(DepthSelectionState {
    expected: None,
    selected_pair: None,
    retired_pairs: BTreeSet::new(),
    pending_explicit_unbind: None,
    ambiguous: false,
});

struct CompletedDepthSnapshots {
    textures: Vec<DepthTexture>,
    pass_draw_calls: Vec<u64>,
    pass_identities: Vec<Option<crate::camera_probe::MainPassIdentity>>,
    next: usize,
    present_seq: u64,
    copies_this_frame: usize,
    exact_snapshot: Option<ExactSnapshotRecord>,
    // Keep the source COM object alive through the Present so the raw identity in
    // `ExactSnapshotRecord` cannot suffer allocator ABA after an unbind.
    exact_snapshot_source: Option<SelectedDepth>,
    exact_snapshot_ambiguous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExactSnapshotRecord {
    texture_index: usize,
    unbind_ordinal_from_start: u32,
    source_resource_tag: u64,
    identity: crate::camera_probe::MainPassIdentity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedExactSnapshot {
    texture_index: usize,
    unbind_ordinal_from_end: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SelectedDsvUnbindCounter {
    present_seq: u64,
    count: u32,
}

static SELECTED_DSV_UNBINDS: Mutex<SelectedDsvUnbindCounter> =
    Mutex::new(SelectedDsvUnbindCounter {
        present_seq: u64::MAX,
        count: 0,
    });

const PASS_IDENTITY_TRACE_SCHEMA_V1: &str = "depth-pass-identity-trace/v1";
const PASS_IDENTITY_TRACE_FILE: &str = "depth-pass-identity-trace.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PassIdentityJoin {
    Exact,
    SelectedSnapshotDiffers,
    ProjectionPassNotSnapshotted,
    ProjectionObservationMissing,
    ExactDrawMissing,
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SnapshotPassIdentityTrace {
    unbind_ordinal_from_start: u32,
    unbind_ordinal_from_end: u32,
    pass_ordinal: Option<u32>,
    draw_calls: u64,
    exact_draw_ordinal: Option<u32>,
    projection_observed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PassIdentityTrace {
    schema: &'static str,
    generation: u64,
    present_seq: u64,
    expected_pass_ordinal: Option<u32>,
    expected_draw_ordinal: Option<u32>,
    exact_draw_seen: bool,
    projection_observed: bool,
    selected_unbind_ordinal_from_end: u32,
    snapshots: Vec<SnapshotPassIdentityTrace>,
    join: PassIdentityJoin,
}

struct PendingCaptureEvidence {
    pass: depth_lab::capture_timing::DsvPassEvidence,
    identity_trace: Option<PassIdentityTrace>,
    /// Selected-present mode records the exact draw projection while the
    /// candidate is materialized; delayed feedback must not depend on a later
    /// camera-cache lookup.
    projection: Option<depth_lab::capture_timing::IndexProjection>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ObservedDrawViewport {
    generation: u64,
    count: u32,
    min_depth: f64,
    max_depth: f64,
}

static COMPLETED_DEPTH_SNAPSHOTS: Mutex<Option<CompletedDepthSnapshots>> = Mutex::new(None);

/// 交给 writer 线程的一帧：**原始字节，未解码**。
///
/// 解码与降采样在 writer 线程做，不占渲染线程——见 [`RawDepthFrame`] 的模块说明。
struct PendingDepthFrame {
    frame: RawDepthFrame,
    readback_latency_frames: u64,
    generation: u64,
    pass: depth_lab::capture_timing::DsvPassEvidence,
    projection: depth_lab::capture_timing::IndexProjection,
    identity_trace: Option<Box<PassIdentityTrace>>,
}

enum DepthWriterCommand {
    Start {
        generation: u64,
        root: std::path::PathBuf,
        projection: depth_lab::capture_timing::IndexProjection,
        d3d12_dsv_id: Option<String>,
        pass_identity_trace: bool,
        reply: SyncSender<Result<(), String>>,
    },
    Frame(PendingDepthFrame),
    Stop {
        generation: u64,
        reply: SyncSender<Result<DepthWriterSummary, String>>,
    },
}

#[derive(Debug, Clone, Default)]
struct DepthWriterSummary {
    row_count: u64,
    file_count: u64,
    last_depth_present_seq: Option<u64>,
    errors: Vec<String>,
}

/// 极简日志：写到 `%TEMP%\dm-depth-probe.log`。
///
/// **不用任何日志框架**：`DllMain` 阶段初始化第三方 logger 有 loader lock 风险，
/// 而 probe 需要的只是「出事时能看到一行字」。真机上这个文件是第一手排障入口。
pub fn log(msg: &str) {
    use std::io::Write;
    let Some(path) = probe_log_path() else {
        return;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[dm-depth-probe] {msg}");
    }
}

/// 日志文件的位置。自动停录要把它快照进诊断包，所以这条路径不能只存在于
/// [`log`] 内部——两处各写一遍迟早会漂成两个文件。
fn probe_log_path() -> Option<std::path::PathBuf> {
    let dir = std::env::var("TEMP").ok()?;
    Some(std::path::Path::new(&dir).join("dm-depth-probe.log"))
}

fn record_camera_draw_entry(method: CameraDrawMethod) {
    if CAMERA_HOOK_ENTRY_COUNTERS.record_draw(method) == 1 {
        log(&format!(
            "camera discovery draw hook first-hit method={}",
            method.label()
        ));
    }
}

fn record_camera_om_bind_entry(method: OmBindMethod) {
    if CAMERA_HOOK_ENTRY_COUNTERS.record_om_bind(method) == 1 {
        log(&format!(
            "camera discovery OM bind hook first-hit method={}",
            method.label()
        ));
    }
}

fn context_vtable_address(context: *mut core::ffi::c_void) -> usize {
    if context.is_null() {
        return 0;
    }
    unsafe { *(context as *mut *mut usize) as usize }
}

fn record_foreign_camera_draw_entry(method: CameraDrawMethod, context: *mut core::ffi::c_void) {
    if CAMERA_HOOK_ENTRY_COUNTERS.record_foreign_draw(method) == 1 {
        log(&format!(
            "camera discovery foreign draw hook first-hit method={} actual_context={:#x} actual_vtable={:#x} expected_context={:#x}",
            method.label(),
            context as usize,
            context_vtable_address(context),
            GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire),
        ));
    }
}

fn record_foreign_camera_om_bind_entry(method: OmBindMethod, context: *mut core::ffi::c_void) {
    if CAMERA_HOOK_ENTRY_COUNTERS.record_foreign_om_bind(method) == 1 {
        log(&format!(
            "camera discovery foreign OM hook first-hit method={} actual_context={:#x} actual_vtable={:#x} expected_context={:#x}",
            method.label(),
            context as usize,
            context_vtable_address(context),
            GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire),
        ));
    }
}

fn reset_camera_hook_entry_counters() {
    let _ = CAMERA_HOOK_ENTRY_COUNTERS.take();
}

fn log_and_reset_camera_hook_entry_counters(phase_id: &str) {
    let snapshot = CAMERA_HOOK_ENTRY_COUNTERS.take();
    let draws = CameraDrawMethod::ALL
        .into_iter()
        .map(|method| format!("{}={}", method.label(), snapshot.draws[method as usize]))
        .collect::<Vec<_>>()
        .join(" ");
    let om_binds = OmBindMethod::ALL
        .into_iter()
        .map(|method| format!("{}={}", method.label(), snapshot.om_binds[method as usize]))
        .collect::<Vec<_>>()
        .join(" ");
    let foreign_draws = CameraDrawMethod::ALL
        .into_iter()
        .map(|method| {
            format!(
                "foreign_{}={}",
                method.label(),
                snapshot.foreign_draws[method as usize]
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let foreign_om_binds = OmBindMethod::ALL
        .into_iter()
        .map(|method| {
            format!(
                "foreign_{}={}",
                method.label(),
                snapshot.foreign_om_binds[method as usize]
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    log(&format!(
        "camera discovery phase={phase_id} hook entry counts: {draws} {om_binds} {foreign_draws} {foreign_om_binds}"
    ));
}

/// EXR 编码 worker 的线程降级：BELOW_NORMAL + EcoQoS。
///
/// exr 的 ZIP16 原本落在 rayon 全局池——逻辑核数个**普通优先级**线程，每次
/// encode 都是 10+ ms 的全核突发，与游戏抢核/带宽/功耗（#504 遥测：encode
/// 重叠帧慢帧风险 3.32×）。现在编码走 depth-lab 的专用小池，每个 worker 在
/// 这里降级：BELOW_NORMAL 让游戏线程先跑但不至于饿死（完整 BACKGROUND 模式
/// 在满载机器上有饿死风险，会顶爆有界队列造成丢帧）；EcoQoS 提示调度器把
/// 线程放到能效核/低频档——deflate 在 E 核上慢一点无所谓，别抢 P 核才是重点。
fn encode_worker_thread_init() {
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        let throttle = THREAD_POWER_THROTTLING_STATE {
            Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        };
        // EcoQoS 在不支持的系统（Win10 旧版）上返回失败，忽略即可——
        // BELOW_NORMAL 已经把调度让位做了，EcoQoS 是频率/能效核的增强项。
        let _ = SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            (&raw const throttle).cast(),
            size_of::<THREAD_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn early_ready_event_name(pid: u32) -> String {
    format!(r"Local\dm-depth-early-ready-{pid}")
}

fn is_missing_early_ready_event(code: HRESULT) -> bool {
    code == HRESULT::from_win32(ERROR_FILE_NOT_FOUND.0)
}

/// Opens the launcher's PID-scoped event when this process was injected while
/// suspended. A missing event is the normal, unchanged D3D11 late-injection
/// path.
fn open_early_ready_event() -> Result<Option<HANDLE>, String> {
    let mut wide = early_ready_event_name(std::process::id())
        .encode_utf16()
        .collect::<Vec<_>>();
    wide.push(0);
    match unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(wide.as_ptr())) } {
        Ok(event) => Ok(Some(event)),
        Err(error) if is_missing_early_ready_event(error.code()) => Ok(None),
        Err(error) => Err(format!("opening early-ready event failed: {error}")),
    }
}

fn signal_early_ready(event: HANDLE) -> Result<(), String> {
    let result = unsafe { SetEvent(event) };
    let close = unsafe { CloseHandle(event) };
    result.map_err(|error| format!("signalling early-ready event failed: {error}"))?;
    close.map_err(|error| format!("closing early-ready event failed: {error}"))?;
    Ok(())
}

fn valid_discovery_launch_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn start_d3d12_discovery_watcher(
    backend: std::sync::Arc<crate::d3d12_backend::D3d12ObservationBackend>,
) -> Result<bool, String> {
    let Some(config_path) = std::env::var_os("DM_DEPTH_DISCOVERY_CONFIG") else {
        return Ok(false);
    };
    let launch_id = std::env::var("DM_DEPTH_LAUNCH_ID")
        .map_err(|_| "D3D12 discovery requires DM_DEPTH_LAUNCH_ID".to_string())?;
    if !valid_discovery_launch_id(&launch_id) {
        return Err("D3D12 discovery launch id is not 32 lowercase hex digits".into());
    }
    let config_path = std::path::PathBuf::from(config_path);
    if !config_path.is_absolute() {
        return Err("D3D12 discovery config path must be absolute".into());
    }
    let config_path = config_path
        .canonicalize()
        .map_err(|error| format!("resolve D3D12 discovery config failed: {error}"))?;
    let config: D3d12DiscoveryConfig = serde_json::from_slice(
        &std::fs::read(&config_path)
            .map_err(|error| format!("read D3D12 discovery config failed: {error}"))?,
    )
    .map_err(|error| format!("parse D3D12 discovery config failed: {error}"))?;
    if config.schema != "dm-cp2077-discovery-smoke-config/v1"
        || config.output_dir != "."
        || config.recording_ready
    {
        return Err("D3D12 discovery config is not the discovery-only v1 contract".into());
    }
    let output_dir = config_path
        .parent()
        .ok_or("D3D12 discovery config has no parent directory")?
        .to_path_buf();
    let stop_path = output_dir.join("discovery-stop-request.json");
    let report_path = output_dir.join("d3d12-observation.json");
    if stop_path.exists() || report_path.exists() {
        return Err("D3D12 discovery directory contains a stale stop/report file".into());
    }
    std::thread::Builder::new()
        .name("dm-d3d12-discovery".into())
        .spawn(move || {
            while !stop_path.is_file() {
                std::thread::sleep(Duration::from_millis(25));
            }
            let report = D3d12DiscoveryReport {
                schema: "dm-d3d12-discovery-observation/v1",
                process_launch_id: launch_id,
                recording_ready: false,
                observation: finalize_d3d12_discovery_snapshot(backend.snapshot()),
            };
            let result = (|| -> Result<(), String> {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&report_path)
                    .map_err(|error| format!("create D3D12 observation failed: {error}"))?;
                let mut writer = BufWriter::new(file);
                serde_json::to_writer_pretty(&mut writer, &report)
                    .map_err(|error| format!("serialize D3D12 observation failed: {error}"))?;
                writer
                    .write_all(b"\n")
                    .map_err(|error| format!("finish D3D12 observation failed: {error}"))?;
                writer
                    .flush()
                    .map_err(|error| format!("flush D3D12 observation failed: {error}"))?;
                writer
                    .get_ref()
                    .sync_all()
                    .map_err(|error| format!("sync D3D12 observation failed: {error}"))?;
                Ok(())
            })();
            if let Err(error) = result {
                log(&error);
            }
        })
        .map_err(|error| format!("start D3D12 discovery watcher failed: {error}"))?;
    Ok(true)
}

/// probe 初始化。在 `DllMain` 起的独立线程里跑，避开 loader lock。
pub fn init() -> Result<(), String> {
    log("init 开始");
    crate::perf::initialize_disabled();
    // 必须在第一次 EXR 编码之前注册：编码池是惰性建的，建成后不再补挂回调。
    depth_lab::exr_out::set_encode_thread_init(encode_worker_thread_init);

    // The launcher creates this event before LoadLibraryW and keeps the game
    // main thread suspended until it is signalled. Device hooks are therefore
    // armed before the application can create D3D12 queues, lists, or DSVs.
    // This proves hook coverage only; it never implies Depth readiness.
    let early_ready_event = open_early_ready_event()?;
    // Discovery is a separate, permanently non-recording process mode. Treat
    // the environment marker itself as fail-closed: even a malformed request
    // must not fall through to a stale legacy config or a StartRun command for
    // a recycled PID.
    let discovery_only = crate::cp2077_pose_wiring::discovery_requested();
    let raw_demo_requested = crate::raw_depth_demo::requested();
    if raw_demo_requested && early_ready_event.is_none() {
        return Err("raw depth demo requires the fail-closed early launcher".into());
    }
    if early_ready_event.is_some() {
        let bootstrap = crate::d3d12_backend::D3d12Bootstrap::default_adapter()?;
        let discovery_backend = std::sync::Arc::clone(bootstrap.backend());
        D3D12_EARLY_BOOTSTRAP
            .set(bootstrap)
            .map_err(|_| "D3D12 early bootstrap was already installed".to_string())?;
        if discovery_only || raw_demo_requested {
            discovery_backend.set_runtime_observation_active(true);
        }
        log("D3D12 early device hooks armed before game resume");
        if start_d3d12_discovery_watcher(discovery_backend)? {
            log("D3D12 discovery observation watcher armed");
        }
        if crate::cp2077_pose_wiring::discovery_requested() {
            let executable_sha256 = current_executable_sha256()?;
            if crate::cp2077_pose_wiring::try_start_from_environment(&executable_sha256, true)? {
                log("CP2077 exact-build pose discovery observer armed");
            }
        }
        if raw_demo_requested {
            let executable_sha256 = current_executable_sha256()?;
            crate::raw_depth_demo::try_start_from_environment(
                &executable_sha256,
                std::sync::Arc::clone(
                    D3D12_EARLY_BOOTSTRAP
                        .get()
                        .ok_or("raw depth demo lost the early D3D12 bootstrap")?
                        .backend(),
                ),
            )?
            .ok_or("raw depth demo environment marker did not activate a controller")?;
            log(
                "CP2077 local non-trainable raw D32 demo controller initialized; awaiting semantic binding and explicit arm request",
            );
        }
    }

    let mut pending_dsv_discovery = None;
    if formal_capture_control_enabled(discovery_only || raw_demo_requested) {
        // Keep the old config/env route for manual experiments. It intentionally
        // does not produce lifecycle ACKs and therefore cannot satisfy the client.
        let legacy_config = initialise_legacy_capture();
        let protocol_start_for_this_process =
            depth_lab::probe_control::load_command_from(&depth_lab::probe_control::control_path())
                .ok()
                .is_some_and(|command| {
                    matches!(
                        command.command,
                        depth_lab::ProbeControlCommand::StartRun(start)
                            if start.target_process_id == std::process::id()
                    )
                });
        if legacy_config
            .as_ref()
            .is_some_and(|loaded| loaded.config.dsv_discovery.is_some())
        {
            let loaded = legacy_config
                .expect("legacy_config was checked immediately before DSV discovery activation");
            if protocol_start_for_this_process {
                return Err(
                    "D3D11 DSV discovery refuses to coexist with a formal StartRun command".into(),
                );
            }
            pending_dsv_discovery = Some((
                loaded.config,
                loaded.source_sha256.is_some(),
                loaded.source_sha256,
            ));
        } else {
            init_depth_worker()?;
            start_capture_control_watcher()?;
            if !protocol_start_for_this_process && let Some(loaded) = legacy_config {
                let config = loaded.config;
                let managed_by_config = depth_lab::probe_config::config_path().exists();
                start_depth_run(
                    &config,
                    1,
                    false,
                    std::env::var_os("DM_DEPTH_PROBE_PASS_IDENTITY_TRACE").is_some(),
                    false,
                    false,
                )?;
                ACTIVE_GENERATION.store(1, Ordering::Release);
                FORMAL_CAPTURE_ARMED.store(true, Ordering::Release);
                DEPTH_CAPTURE_ENABLED.store(true, Ordering::Release);
                if managed_by_config {
                    start_legacy_config_watcher(config.session_id.clone())?;
                }
            }
        }
    } else {
        log("discovery-only process: formal capture control and legacy capture are disabled");
    }

    let (swapchain, _device, _ctx, hwnd) = create_dummy_swapchain()
        .map_err(|e| format!("创建临时 swapchain 失败（用于读取 vtable 布局）: {e}"))?;

    let raw = swapchain.as_raw();

    // §5.2「vtable 索引不得硬编码」的落地：核对那一项确实来自 dxgi.dll。
    verify_layout(raw, dxgi_swapchain::PRESENT, "dxgi.dll")?;
    log("vtable 布局核对通过：Present 落在 dxgi.dll");

    let hook = unsafe {
        VtableHook::install(
            raw,
            dxgi_swapchain::PRESENT,
            hooked_present as *const () as usize,
        )
        .map_err(|e| format!("安装 Present hook 失败: {e}"))?
    };
    ORIGINAL_PRESENT.store(hook.original(), Ordering::SeqCst);
    let _ = PRESENT_HOOK.set(hook);

    // The swapchain address is only a registry key. Hook Release on the same
    // shared DXGI vtable so a destroyed D3D12 swapchain cannot ABA-reuse a
    // stale queue/device association.
    let release_hook = unsafe {
        VtableHook::install(raw, 2, hooked_swapchain_release as *const () as usize)
            .map_err(|e| format!("install swapchain Release hook failed: {e}"))?
    };
    ORIGINAL_SWAPCHAIN_RELEASE.store(release_hook.original(), Ordering::SeqCst);
    let _ = SWAPCHAIN_RELEASE_HOOK.set(release_hook);

    // ResizeBuffers 必须一起挂——不是为了「水印别丢」，是为了**别把游戏弄坏**：
    // DXGI 要求调用 ResizeBuffers 前释放掉所有后缓冲引用，否则直接返回
    // DXGI_ERROR_INVALID_CALL。watermark_gpu 缓存了后缓冲的 RTV，就必须在它执行前放手。
    //
    // 漏了这条的症状**不是**「水印丢了」，是「装了 probe 就改不了分辨率」，
    // 而且那个 HRESULT 由游戏收到，probe 日志里一片干净（77 号 R-24）。
    verify_layout(raw, dxgi_swapchain::RESIZE_BUFFERS, "dxgi.dll")?;
    let resize_hook = unsafe {
        VtableHook::install(
            raw,
            dxgi_swapchain::RESIZE_BUFFERS,
            hooked_resize_buffers as *const () as usize,
        )
        .map_err(|e| format!("安装 ResizeBuffers hook 失败: {e}"))?
    };
    ORIGINAL_RESIZE_BUFFERS.store(resize_hook.original(), Ordering::SeqCst);
    let _ = RESIZE_BUFFERS_HOOK.set(resize_hook);
    log("vtable 布局核对通过：ResizeBuffers 落在 dxgi.dll，hook 已安装");

    // DSV discovery is armed only after the Present hook is live. This keeps the evidence window
    // honest: a failed hook bootstrap cannot leave behind an apparently complete report. It also
    // bypasses the formal control/writer path entirely, so this process mode has no Ready emitter.
    if let Some((config, managed_by_config, probe_config_sha256)) = pending_dsv_discovery {
        let discovery = config
            .dsv_discovery
            .clone()
            .ok_or("D3D11 DSV discovery config disappeared before activation")?;
        let actual_executable_sha256 = current_executable_sha256()?;
        DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
        DSV_DISCOVERY_DEFERRED_WARMUP_STARTED.store(false, Ordering::Release);
        DSV_DISCOVERY_OBSERVATION_COMPLETE.store(false, Ordering::Release);
        DSV_DISCOVERY_RETAINED_FRAMES.store(0, Ordering::Release);
        DSV_DISCOVERY_FRAME_LIMIT.store(discovery.limits.max_frames, Ordering::Release);
        DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.store(false, Ordering::Release);
        dsv_frame_tracker()
            .lock()
            .map_err(|_| "D3D11 DSV discovery tracker lock poisoned during activation")?
            .reset_for_discovery();
        *ACTIVE_CONFIG
            .lock()
            .map_err(|_| "D3D11 DSV discovery active config lock poisoned")? = Some(config.clone());
        if let Err(error) = crate::dsv_discovery::start(
            discovery.clone(),
            std::path::Path::new(&config.output_dir),
            &actual_executable_sha256,
        ) {
            if let Ok(mut tracker) = dsv_frame_tracker().lock() {
                tracker.reset();
            }
            DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
            if let Ok(mut active) = ACTIVE_CONFIG.lock() {
                *active = None;
            }
            return Err(error);
        }
        ACTIVE_GENERATION.store(1, Ordering::Release);
        FORMAL_CAPTURE_ARMED.store(false, Ordering::Release);
        DEPTH_CAPTURE_ENABLED.store(true, Ordering::Release);
        log("D3D11 DSV discovery_only 已启用；formal writer/Ready/profile/binding 均未启动");
        if managed_by_config
            && let Err(error) = start_legacy_config_watcher(config.session_id.clone())
        {
            DEPTH_CAPTURE_ENABLED.store(false, Ordering::Release);
            let seal = crate::dsv_discovery::stop()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|seal_error| format!("failed: {seal_error}"));
            if let Ok(mut tracker) = dsv_frame_tracker().lock() {
                tracker.reset();
            }
            DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
            if let Ok(mut active) = ACTIVE_CONFIG.lock() {
                *active = None;
            }
            return Err(format!(
                "D3D11 DSV discovery watcher failed: {error}; empty discovery sealed at {seal}"
            ));
        }
        if managed_by_config {
            let activation_result = probe_config_sha256.as_deref().map_or_else(
                || Err("managed D3D11 DSV discovery lost the exact probe-config digest".into()),
                |probe_config_sha256| {
                    crate::dsv_discovery::write_activation_ack(
                        &discovery,
                        &config.session_id,
                        std::path::Path::new(&config.output_dir),
                        &actual_executable_sha256,
                        probe_config_sha256,
                        true,
                    )
                },
            );
            if let Err(error) = activation_result {
                DEPTH_CAPTURE_ENABLED.store(false, Ordering::Release);
                let seal = crate::dsv_discovery::stop()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|seal_error| format!("failed: {seal_error}"));
                if let Ok(mut tracker) = dsv_frame_tracker().lock() {
                    tracker.reset();
                }
                DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
                if let Ok(mut active) = ACTIVE_CONFIG.lock() {
                    *active = None;
                }
                return Err(format!(
                    "D3D11 DSV discovery activation ACK failed: {error}; discovery sealed at {seal}"
                ));
            }
            log("D3D11 DSV discovery probe-authored activation ACK sealed");
        }
    }

    // 临时窗口只为取 vtable，取完即毁；hook 装在 vtable 上，对游戏自己的
    // swapchain 同样生效（同一个 vtable 被该进程所有 IDXGISwapChain 实例共享）。
    unsafe {
        let _ = DestroyWindow(hwnd);
    }

    if let Some(event) = early_ready_event {
        signal_early_ready(event)?;
        log("D3D12 early hooks armed; launcher resume signalled");
    }

    log("Present hook 已安装，等待游戏出帧");
    Ok(())
}

const fn formal_capture_control_enabled(experimental_only: bool) -> bool {
    !experimental_only
}

/// 我们替换进 vtable 的 `Present`。
///
/// **签名必须与 `IDXGISwapChain::Present` 完全一致**（`extern "system"`，
/// 三个参数，返回 `HRESULT` 的裸 i32）。签名不一致会破坏调用栈。
/// 合并 GPU 回读与 writer 两层的交付统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeliveryStats {
    readback: readback::ReadbackStats,
    writer_queue_full: u64,
    writer_bypassed: u64,
}

impl DeliveryStats {
    /// 实际落盘的比例。
    ///
    /// writer 队列丢弃的帧**已经包含在 `completed` 里**（GPU 回读确实成功了），
    /// 所以只从分子扣除，**不能再加进分母**——否则同一帧被算两次。
    fn delivery_rate(self) -> f64 {
        let delivered = self
            .readback
            .completed
            .saturating_sub(self.writer_queue_full)
            .saturating_sub(self.writer_bypassed);
        let total = self
            .readback
            .completed
            .saturating_add(self.readback.drops.total());
        if total == 0 {
            1.0
        } else {
            delivered as f64 / total as f64
        }
    }
}

fn delivery_stats() -> Option<DeliveryStats> {
    Some(DeliveryStats {
        readback: readback::stats()?,
        writer_queue_full: WRITER_QUEUE_DROPPED.load(Ordering::Relaxed),
        writer_bypassed: WRITER_BYPASSED.load(Ordering::Relaxed),
    })
}

/// 在每次 `Present` 的**末尾**推进节拍，算出**下一帧**要不要采。
///
/// # 为什么算的是下一帧而不是本帧
///
/// 采一帧深度要两步，发生在**不同时刻**：
///
/// ```text
/// Present(N-1) ─┬─► 游戏渲染第 N 帧
///               │     └─ OMSetRenderTargets 把 DSV 快照送进环   ← 需要第 N 帧的结论
///               └─► Present(N)
///                     └─ capture_last_completed_at_present(N)   ← 同样需要第 N 帧的结论
/// ```
///
/// 第一步在 `Present(N)` **之前**就发生了。所以第 N 帧的结论必须在
/// `Present(N-1)` 末尾就定下来。第 0 帧因此永远不采——少一帧无所谓，
/// 把这个特例写进代码只会多一条没人测过的分支。
fn update_cadence_for_next_frame() {
    let Ok(mut guard) = CADENCE.lock() else {
        return;
    };
    let cadence = match guard.as_mut() {
        Some(c) => c,
        None => {
            let cadence_mode = resolved_depth_cadence();
            if cadence_mode == depth_lab::DepthCadence::EveryPresent {
                log("depth cadence=every_present：逐 Present 采集，不启用 QPC 限流");
                guard.insert(depth_lab::cadence::Cadence::every_present())
            } else {
                let hz = TARGET_DEPTH_HZ
                    .lock()
                    .ok()
                    .and_then(|value| *value)
                    .or_else(|| resolved_config().and_then(|config| config.target_depth_hz))
                    .unwrap_or_else(|| {
                        log(&format!(
                            "配置未给 target_depth_hz，兜底 {FALLBACK_TARGET_HZ} Hz"
                        ));
                        FALLBACK_TARGET_HZ
                    });
                let mut freq = 0i64;
                if unsafe { QueryPerformanceFrequency(&mut freq) }.is_err() || freq <= 0 {
                    log("QueryPerformanceFrequency 失败，节拍器无法建立，本次不采深度");
                    return;
                }
                match depth_lab::cadence::Cadence::new(hz, freq as u64) {
                    Ok(c) => guard.insert(c),
                    Err(e) => {
                        log(&format!("节拍器建立失败（{e}），本次不采深度"));
                        return;
                    }
                }
            }
        }
    };
    let now = if cadence.is_every_present() {
        0
    } else {
        let mut now = 0i64;
        if unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
            return;
        }
        now as u64
    };
    // PRESENT_SEQ 在本次 hook 开头已自增，此刻它的值就是**下一帧**的 seq。
    // profile 可显式要求逐 Present；旧 profile 则继续按目标 Hz 的时间基限流。
    let next_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    let take = cadence.should_capture_frame(next_seq, now);
    CAPTURE_THIS_FRAME.store(take, Ordering::Release);
    if !CADENCE_FIRST_DECISION_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "depth cadence first decision: next_present_seq={next_seq} take={take} every_present={} issued={} max_frames={:?}",
            cadence.is_every_present(),
            CAPTURE_ISSUED.load(Ordering::Acquire),
            capture_max_frames()
        ));
    }
}

/// 逐 Present 观测游戏帧率是否锁在车道帧率（#801 修复清单第 2 项）。
///
/// 与节拍器同一时刻取样（original Present 返回之后），但独立于采集判定：
/// 节拍器答「这帧采不采」，本闸答「这条数据可不可信」。violated 只在此处
/// 记一次日志；裁决落盘在 StopRun 处，deliver 读到 violated 即拒绝出件。
fn observe_present_rate() {
    let lane = LANE_FPS.load(Ordering::Acquire);
    if lane == 0 {
        if !LANE_ABSENT_LOGGED.swap(true, Ordering::AcqRel) {
            log("StartRun 未带 lane_fps，present 帧率闸停用（旧客户端/手工流程）");
        }
        return;
    }
    let Ok(mut guard) = PRESENT_RATE_GATE.lock() else {
        return;
    };
    if guard.is_none() {
        let mut freq = 0i64;
        if unsafe { QueryPerformanceFrequency(&mut freq) }.is_err() || freq <= 0 {
            return;
        }
        match depth_lab::present_rate::PresentRateGate::new(lane, freq as u64) {
            Ok(gate) => {
                guard.replace(gate);
            }
            Err(error) => {
                log(&format!("present 帧率闸建立失败（{error}），本次停用"));
                LANE_FPS.store(0, Ordering::Release);
                return;
            }
        }
    }
    let Some(gate) = guard.as_mut() else {
        return;
    };
    let mut now = 0i64;
    if unsafe { QueryPerformanceCounter(&mut now) }.is_err() {
        return;
    }
    gate.on_present(now);
    if gate.violated() && !RATE_VIOLATION_LOGGED.swap(true, Ordering::AcqRel) {
        let verdict = gate.verdict();
        log(&format!(
            "⚠️ 游戏未锁车道帧率：观测 {:.2} fps vs 车道 {} fps（偏差 {:.1}%，容差 {:.0}%）。\
             深度与 15fps 交付网格将错位，本条会被标记不可交付（#801）",
            verdict.observed_fps_at_worst,
            verdict.lane_fps,
            verdict.worst_deviation_pct,
            depth_lab::present_rate::RATE_TOLERANCE_PCT,
        ));
    }
}

/// 我们替换进 vtable 的 `ResizeBuffers`。
///
/// 只做一件事：**在真正的 ResizeBuffers 之前**丢掉 `watermark_gpu` 缓存的后缓冲 RTV。
///
/// 顺序不能反。事后再清等于已经让游戏的分辨率切换失败了一次，
/// 而失败的 HRESULT 是**游戏**收到的——我们的日志里什么都看不到。
///
/// 签名必须与 `IDXGISwapChain::ResizeBuffers` 完全一致：
/// `(This, BufferCount, Width, Height, NewFormat, SwapChainFlags) -> HRESULT`。
unsafe extern "system" fn hooked_resize_buffers(
    swapchain: *mut core::ffi::c_void,
    buffer_count: u32,
    width: u32,
    height: u32,
    new_format: i32,
    swapchain_flags: u32,
) -> i32 {
    if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) && formal_capture_armed() {
        // ResizeBuffers may run on a different thread. Do not touch either SPSC
        // endpoint here; the sole Present-thread owner publishes the fault.
        SELECTED_PRESENT_RESIZE_FAULT.store(true, Ordering::Release);
    }
    let is_d3d12 = D3D12_EARLY_BOOTSTRAP
        .get()
        .is_some_and(|bootstrap| bootstrap.backend().is_d3d12_swapchain(swapchain));
    if is_d3d12 {
        let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() else {
            return -1;
        };
        if let Err(error) = bootstrap.backend().prepare_swapchain_resize(swapchain) {
            log(&format!(
                "D3D12 ResizeBuffers rejected fail-closed: {error}"
            ));
            return -1;
        }
    } else {
        crate::watermark_gpu::invalidate();
    }

    let original = ORIGINAL_RESIZE_BUFFERS.load(Ordering::SeqCst);
    if original == 0 {
        return -1; // E_FAIL：拿不到原函数就不能假装成功
    }
    type ResizeBuffersFn =
        unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32, u32, i32, u32) -> i32;
    let original: ResizeBuffersFn = unsafe { core::mem::transmute(original) };
    unsafe {
        original(
            swapchain,
            buffer_count,
            width,
            height,
            new_format,
            swapchain_flags,
        )
    }
}

unsafe extern "system" fn hooked_swapchain_release(swapchain: *mut core::ffi::c_void) -> u32 {
    let original = ORIGINAL_SWAPCHAIN_RELEASE.load(Ordering::SeqCst);
    if original == 0 {
        return 0;
    }
    let original: unsafe extern "system" fn(*mut core::ffi::c_void) -> u32 =
        unsafe { core::mem::transmute(original) };
    let remaining = unsafe { original(swapchain) };
    if remaining == 0
        && let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get()
    {
        bootstrap.backend().unregister_swapchain(swapchain);
    }
    remaining
}

fn queue_d3d12_depth_frames(
    swapchain: *mut core::ffi::c_void,
    current_present_seq: u64,
    callback_generation: u64,
    formal: bool,
) -> Result<usize, String> {
    if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
        return queue_d3d12_selected_frames(
            swapchain,
            current_present_seq,
            callback_generation,
            formal,
        );
    }
    let bootstrap = D3D12_EARLY_BOOTSTRAP
        .get()
        .ok_or("D3D12 Present has no retained early bootstrap")?;
    if crate::raw_depth_demo::camera_archive_enabled() {
        crate::raw_depth_demo::selected_on_present(
            bootstrap.backend(),
            swapchain,
            current_present_seq,
        )?;
        if crate::raw_depth_demo::selected_feedback_enabled() {
            return Ok(0);
        }
    }
    if !bootstrap.backend().capture_is_armed() {
        return Ok(0);
    }
    let ready = bootstrap.backend().drain_depth_capture()?;
    if ready.is_empty() {
        return Ok(0);
    }
    let profile_id = D3D12_DEPTH_PROFILE
        .lock()
        .map_err(|_| "D3D12 depth profile lock poisoned during drain".to_string())?
        .as_ref()
        .map(|(selector, _)| selector.candidate_id.clone())
        .ok_or("D3D12 capture is armed without an exact profile")?;
    let raw_demo = crate::raw_depth_demo::active();
    // The local raw demo is deliberately the evidence-gathering path used
    // before a production projection has been authored.  Its D32 bytes and
    // exact Present/QPC identity must therefore be drained without consulting
    // RUN_PROJECTION.  Requiring active_projection here made a successfully
    // armed CP2077 capture silently discard every completed readback with
    // "run projection is not initialized".
    if d3d12_raw_drain_bypasses_formal_projection(raw_demo) {
        let mut paired = 0;
        for frame in ready {
            if !bootstrap
                .backend()
                .watermark_submitted_for(swapchain, frame.present_seq)
            {
                return Err(format!(
                    "D3D12 depth present_seq {} has no submitted RGB watermark",
                    frame.present_seq
                ));
            }
            paired += 1;
            crate::raw_depth_demo::submit_frame(frame, current_present_seq)?;
        }
        return Ok(paired);
    }
    let tx = formal
        .then(|| DEPTH_TX.get().ok_or("depth writer is not initialized"))
        .transpose()?;
    let _send_guard = formal
        .then(|| {
            DEPTH_SEND_LOCK
                .try_lock()
                .map_err(|_| "late D3D12 depth callback raced with writer stop".to_string())
        })
        .transpose()?;
    let conv = active_projection(callback_generation)?.map_or(ZConvention::Traditional, |p| p.conv);
    let mut paired = 0;
    for frame in ready {
        if !bootstrap
            .backend()
            .watermark_submitted_for(swapchain, frame.present_seq)
        {
            return Err(format!(
                "D3D12 depth present_seq {} has no submitted RGB watermark",
                frame.present_seq
            ));
        }
        if !formal {
            paired += 1;
            continue;
        }
        let present_seq = frame.present_seq;
        if present_seq < D3D12_FORMAL_START_PRESENT.load(Ordering::Acquire) {
            continue;
        }
        let projection = observed_index_projection(callback_generation, present_seq)?;
        let raw = frame.into_raw_depth_frame(DELIVERY_WIDTH, DELIVERY_HEIGHT, conv);
        let pass = depth_lab::capture_timing::DsvPassEvidence {
            unbind_ordinal_from_end: 1,
            unbind_total: 1,
            draw_calls: 0,
            selection: depth_lab::DsvSelectionAudit {
                selected_id: profile_id.clone(),
                ranked_top_id: Some(profile_id.clone()),
                score_margin: None,
                selected_matches_top: true,
                ranking_confident: true,
                any_msaa: false,
                operator_exact_pin: true,
            },
        };
        let pending = PendingDepthFrame {
            frame: raw,
            readback_latency_frames: current_present_seq.saturating_sub(present_seq),
            generation: callback_generation,
            pass,
            projection,
            identity_trace: None,
        };
        WRITER_QUEUE_DEPTH.fetch_add(1, Ordering::AcqRel);
        match tx
            .ok_or("depth writer is not initialized")?
            .try_send(DepthWriterCommand::Frame(pending))
        {
            Ok(()) => {
                CAPTURE_ISSUED.fetch_add(1, Ordering::AcqRel);
                paired += 1;
            }
            Err(TrySendError::Full(_)) => {
                WRITER_QUEUE_DEPTH.fetch_sub(1, Ordering::AcqRel);
                WRITER_QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed);
                return Err("D3D12 depth writer queue is full".into());
            }
            Err(TrySendError::Disconnected(_)) => {
                WRITER_QUEUE_DEPTH.fetch_sub(1, Ordering::AcqRel);
                return Err("D3D12 depth writer disconnected".into());
            }
        }
    }
    Ok(paired)
}

fn d3d12_selected_in_flight() -> usize {
    SELECTED_PRESENT_RUNTIME
        .lock()
        .ok()
        .and_then(|runtime| {
            runtime
                .as_ref()
                .and_then(|runtime| runtime.d3d12_gpu_ring.as_ref().map(|ring| ring.in_flight()))
        })
        .unwrap_or(0)
}

fn queue_d3d12_selected_frames(
    swapchain: *mut core::ffi::c_void,
    current_present_seq: u64,
    generation: u64,
    formal: bool,
) -> Result<usize, String> {
    let backend = D3D12_EARLY_BOOTSTRAP
        .get()
        .ok_or("D3D12 bootstrap missing")?
        .backend();
    if !backend.capture_is_armed() {
        return Ok(0);
    }
    let _send_guard = DEPTH_SEND_LOCK
        .lock()
        .map_err(|_| "D3D12 depth send lock poisoned")?;
    if generation == 0 || ACTIVE_GENERATION.load(Ordering::Acquire) != generation {
        return Err("stale D3D12 candidate generation".into());
    }
    let mut guard = SELECTED_PRESENT_RUNTIME
        .lock()
        .map_err(|_| "selected-Present runtime poisoned")?;
    let runtime = guard
        .as_mut()
        .ok_or("D3D12 selected-Present runtime missing")?;
    let profile_id = D3D12_DEPTH_PROFILE
        .lock()
        .map_err(|_| "D3D12 profile lock poisoned")?
        .as_ref()
        .map(|(selector, _)| selector.candidate_id.clone())
        .ok_or("D3D12 profile missing")?;
    if runtime.d3d12_gpu_ring.is_none() {
        let projection = active_projection(generation)?.ok_or("D3D12 projection missing")?;
        let conv = projection.conv;
        let device = backend.swapchain_device(swapchain)?;
        let queue = backend.swapchain_queue(swapchain)?;
        runtime.d3d12_gpu_ring = Some(crate::d3d12_candidate::D3d12CandidateRing::new(
            &device,
            &queue,
            &runtime.bootstrap,
            runtime.readback_slots,
            backend.gpu_snapshot_storage_bytes()?,
            conv,
        )?);
        runtime.candidate_conv = Some(conv);
    }
    let ready = runtime
        .d3d12_gpu_ring
        .as_mut()
        .ok_or("D3D12 candidate ring missing")?
        .ready()?;
    let snapshots = backend.take_gpu_depth_snapshots()?;
    let accepting =
        formal && selected_present_accepting_candidates() && !DRAINING.load(Ordering::Acquire);
    let warming = !formal && depth_capture_enabled() && !DRAINING.load(Ordering::Acquire);
    if accepting && (snapshots.len() != 1 || backend.capture_pool_full_count() != 0) {
        return Err(
            "D3D12 candidate interval missed its unique GPU snapshot or exhausted the source ring"
                .into(),
        );
    }
    let mut paired = 0;
    for snapshot in snapshots {
        if !ready || (!warming && !accepting) {
            backend.release_gpu_depth_snapshot(&snapshot, None)?;
            continue;
        }
        // Every candidate is materialized at its own Present, before Select/Retire are consumed.
        if snapshot.present_seq != current_present_seq
            || !backend.watermark_submitted_for(swapchain, snapshot.present_seq)
        {
            return Err(format!(
                "D3D12 snapshot {} lacks the exact current Present/watermark {}",
                snapshot.present_seq, current_present_seq
            ));
        }
        let projection = accepting
            .then(|| observed_index_projection(generation, snapshot.present_seq))
            .transpose()?;
        let slot = if accepting {
            runtime
                .ring_state
                .materialize(snapshot.present_seq)
                .map_err(|fault| {
                    latch_selected_present_fault(fault.reason, fault.to_string());
                    fault.to_string()
                })?
        } else {
            0
        };
        let completion = runtime
            .d3d12_gpu_ring
            .as_mut()
            .ok_or("D3D12 candidate ring missing")?
            .materialize(&snapshot, slot)?;
        backend.release_gpu_depth_snapshot(&snapshot, Some(completion))?;
        paired += 1;
        if accepting {
            let evidence = PendingCaptureEvidence {
                pass: depth_lab::capture_timing::DsvPassEvidence {
                    unbind_ordinal_from_end: 1,
                    unbind_total: 1,
                    draw_calls: 0,
                    selection: depth_lab::DsvSelectionAudit {
                        selected_id: profile_id.clone(),
                        ranked_top_id: Some(profile_id.clone()),
                        score_margin: None,
                        selected_matches_top: true,
                        ranking_confident: true,
                        any_msaa: false,
                        operator_exact_pin: true,
                    },
                },
                projection,
                identity_trace: None,
            };
            if runtime
                .candidate_evidence
                .insert(snapshot.present_seq, evidence)
                .is_some()
            {
                return Err("duplicate D3D12 candidate evidence".into());
            }
        }
    }
    let frames = runtime
        .d3d12_gpu_ring
        .as_mut()
        .ok_or("D3D12 candidate ring missing")?
        .drain_ready()?;
    drop(guard);
    queue_ready_depth_frames(
        frames,
        current_present_seq,
        generation,
        DEPTH_TX.get().ok_or("depth writer not initialized")?,
    )?;
    Ok(paired)
}

/// Raw D32 is the evidence used to author a projection; it cannot depend on a
/// projection already existing. Keep this as an explicit, testable route gate
/// instead of relying on statement ordering in the Present hook.
const fn d3d12_raw_drain_bypasses_formal_projection(raw_demo: bool) -> bool {
    raw_demo
}

fn lock_present_observation(
    gate: &Mutex<()>,
    bypass: impl Fn() -> bool,
) -> Option<std::sync::MutexGuard<'_, ()>> {
    if bypass() {
        return None;
    }
    let guard = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Teardown may retire the generation after our fast check but before this lock.
    (!bypass()).then_some(guard)
}

fn lock_d3d12_teardown(gate: &Mutex<()>, d3d12: bool) -> Option<std::sync::MutexGuard<'_, ()>> {
    d3d12.then(|| {
        gate.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    })
}

fn lock_current_present_completion<'a>(
    gate: &'a Mutex<()>,
    active_generation: &AtomicU64,
    callback_generation: u64,
) -> Option<std::sync::MutexGuard<'a, ()>> {
    let guard = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    (active_generation.load(Ordering::Acquire) == callback_generation).then_some(guard)
}

unsafe fn hooked_present_d3d12(
    swapchain: *mut core::ffi::c_void,
    sync_interval: u32,
    flags: u32,
    seq: u64,
    callback_generation: u64,
    present_run_guard: std::sync::MutexGuard<'_, ()>,
    mut perf_frame: PresentPerfFrame,
) -> i32 {
    let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() else {
        return -1;
    };
    let backend = bootstrap.backend();
    D3D12_ACTIVE_SWAPCHAIN.store(swapchain as usize, Ordering::Release);
    if d3d12_presentation_only() {
        let enabled = depth_capture_enabled();
        let formal = enabled && formal_capture_armed();
        let watermark_required = enabled && !present_only();
        let adapter_observed = if enabled && callback_generation != 0 {
            match observe_swapchain_adapter(swapchain) {
                Ok(()) => true,
                Err(error) => {
                    record_capture_warmup_error(error);
                    false
                }
            }
        } else {
            false
        };
        let watermark_written = if watermark_required {
            match backend.before_present(
                swapchain,
                seq,
                true,
                WATERMARK_ORIGIN.0,
                WATERMARK_ORIGIN.1,
            ) {
                Ok(observation) => observation.watermark_submitted,
                Err(error) => {
                    record_capture_warmup_error(error);
                    false
                }
            }
        } else {
            false
        };
        drop(present_run_guard);
        let original = ORIGINAL_PRESENT.load(Ordering::SeqCst);
        if original == 0 {
            return -1;
        }
        let original: unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32) -> i32 =
            unsafe { core::mem::transmute(original) };
        let perf_pre_ns = perf_frame.pre_ns();
        let result = unsafe { original(swapchain, sync_interval, flags) };
        let perf_post_started = perf_frame.start_post();
        // StopRun shares this lock: a callback returning after Stop cannot
        // arm a new generation or move its accepted Present boundary.
        let _guard = PRESENT_RUN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if enabled
            && depth_capture_enabled()
            && callback_generation != 0
            && ACTIVE_GENERATION.load(Ordering::Acquire) == callback_generation
        {
            let observation = CaptureWarmupEvidence::Presentation {
                adapter_observed,
                present_succeeded: result == 0,
                watermark_required,
                watermark_written,
            };
            if formal {
                if observation.is_ready() {
                    D3D12_PRESENTATION_LAST_PRESENT.store(seq, Ordering::Release);
                    let _ = FIRST_PRESENT_SEQ.compare_exchange(
                        u64::MAX,
                        seq,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                } else {
                    latch_selected_present_fault(
                        depth_lab::selected_present_feedback::FaultReason::TransportCorrupt,
                        format!(
                            "D3D12 presentation stage failed: {}",
                            observation.blockers().join(", ")
                        ),
                    );
                }
            } else if observe_capture_warmup(callback_generation, observation)
                && let Err(error) = arm_formal_capture(callback_generation)
            {
                record_capture_warmup_error(error);
            }
            if should_observe_formal_present_rate(depth_capture_enabled(), formal_capture_armed()) {
                observe_present_rate();
            }
            commit_present_perf(seq, &mut perf_frame, perf_pre_ns, perf_post_started);
        }
        return result;
    }
    let raw_demo = crate::raw_depth_demo::active();
    let raw_demo_present_qpc = raw_demo
        .then(|| crate::raw_depth_demo::note_present(seq))
        .flatten();
    let capture_enabled = depth_capture_enabled() || raw_demo;
    let formal = capture_enabled && formal_capture_armed();
    let selected = SELECTED_PRESENT_ENABLED.load(Ordering::Acquire);
    if selected && SELECTED_PRESENT_RESIZE_FAULT.swap(false, Ordering::AcqRel) {
        latch_selected_present_fault(
            depth_lab::selected_present_feedback::FaultReason::ResourceEpochMismatch,
            "D3D12 ResizeBuffers changed the selected-Present resource epoch",
        );
    }
    if capture_enabled
        && callback_generation != 0
        && let Err(error) = observe_swapchain_adapter(swapchain)
    {
        record_capture_warmup_error(error);
    }

    if raw_demo {
        let resolved = crate::raw_depth_demo::selector_for_arm(backend, seq);
        if let Some((selector, slots)) = resolved {
            if let Ok(mut profile) = D3D12_DEPTH_PROFILE.try_lock() {
                *profile = Some((selector.clone(), slots));
                let gpu_only = crate::raw_depth_demo::selected_feedback_enabled();
                let armed = if backend.capture_is_armed()
                    && crate::raw_depth_demo::authoring_windows_enabled()
                {
                    backend.rebind_authoring_source(&selector)
                } else {
                    crate::raw_depth_demo::selected_preflight(backend, swapchain).and_then(|_| {
                        backend.arm_unique_depth_capture_storage(&selector, slots, gpu_only)
                    })
                }
                .and_then(|source| crate::raw_depth_demo::authoring_source_prepared(source, seq));
                if let Err(error) = armed {
                    crate::raw_depth_demo::record_fault(format!(
                        "raw D32 candidate arm failed after semantic binding: {error}"
                    ));
                }
            } else {
                crate::raw_depth_demo::record_fault(
                    "D3D12 raw demo profile lock poisoned during semantic binding",
                );
            }
        }
    } else if capture_enabled && !backend.capture_is_armed() {
        let profile = D3D12_DEPTH_PROFILE
            .try_lock()
            .ok()
            .and_then(|profile| profile.clone());
        if let Some((selector, slots)) = profile {
            let allocation_check = if selected {
                (|| {
                    let device = backend.swapchain_device(swapchain)?;
                    let guard = SELECTED_PRESENT_RUNTIME
                        .lock()
                        .map_err(|_| "selected-Present runtime poisoned")?;
                    let runtime = guard.as_ref().ok_or("selected-Present runtime missing")?;
                    crate::d3d12_candidate::preflight_snapshot_allocation(
                        &device,
                        &runtime.bootstrap,
                        selector.width,
                        selector.height,
                        slots,
                    )
                })()
            } else {
                Ok(())
            };
            if let Err(error) = allocation_check {
                record_capture_warmup_error(error);
            } else {
                // Discovery may still be warming up; failure to resolve is reported by the
                // existing bounded warmup timeout, not replaced with a guessed resource.
                let result = backend.arm_unique_depth_capture_storage(&selector, slots, selected);
                if let Ok(mut error) = D3D12_LAST_ARM_ERROR.lock() {
                    *error = result.err();
                }
            }
        }
    }

    let watermark_required = capture_enabled
        && backend.capture_is_armed()
        && crate::raw_depth_demo::authoring_camera_interval_active()
        && (!formal || !present_only());
    let present_observation = backend.before_present(
        swapchain,
        seq,
        watermark_required,
        WATERMARK_ORIGIN.0,
        WATERMARK_ORIGIN.1,
    );
    let watermark_written = present_observation
        .as_ref()
        .is_ok_and(|observation| observation.watermark_submitted);
    if let Err(error) = &present_observation {
        if raw_demo {
            crate::raw_depth_demo::record_fault(format!(
                "D3D12 raw demo pre-Present failed: {error}"
            ));
        } else {
            record_capture_warmup_error(format!("D3D12 pre-Present failed: {error}"));
        }
    }
    let kcd2_live_pose_enabled = crate::kcd2_live_pose::enabled();
    let (d3d12_camera_present_submitted, d3d12_viewport_ready) =
        if capture_enabled && !raw_demo && !kcd2_live_pose_enabled {
            match backend
                .take_production_camera(seq)
                .and_then(|row| match row {
                    Some(sample) => submit_d3d12_camera_sample(
                        sample,
                        selected,
                        selected_present_accepting_candidates(),
                    )
                    .map(|submitted| (submitted, true)),
                    None if formal => Err(format!("D3D12 camera is missing at Present {seq}")),
                    None => Ok((false, false)),
                }) {
                Ok(outcome) => outcome,
                Err(error) => {
                    record_capture_warmup_error(error.clone());
                    crate::camera_probe::fail_depth_projection(&error);
                    (false, false)
                }
            }
        } else {
            (false, false)
        };
    // Present(N) submits watermark N before draining the frame-N capture. The
    // capture was inserted before the selected depth first left DEPTH_WRITE,
    // after rendering frame N and before this Present callback.
    let paired = match queue_d3d12_depth_frames(swapchain, seq, callback_generation, formal) {
        Ok(paired) => paired,
        Err(error) => {
            if raw_demo {
                crate::raw_depth_demo::record_fault(format!(
                    "D3D12 raw demo depth drain failed: {error}"
                ));
            }
            if !DRAIN_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                log(&format!("D3D12 non-blocking depth drain failed: {error}"));
            }
            if !raw_demo {
                crate::camera_probe::fail_depth_projection(&error);
                if selected {
                    latch_selected_present_fault(
                        depth_lab::selected_present_feedback::FaultReason::ReadbackMapFailed,
                        error,
                    );
                }
            }
            0
        }
    };
    if !formal && paired != 0 {
        D3D12_PAIRED_WARMUP_FRAMES.fetch_add(paired as u32, Ordering::AcqRel);
    }
    // Join pose/render-camera evidence only after the same-Present depth drain has published its
    // selected-copy identity. Calling before the drain would permanently observe
    // `selected_present_identity_passed=false` for every exact Present.
    crate::cp2077_pose_wiring::observe_present(seq, raw_demo_present_qpc, backend);

    let kcd2_camera_present_submitted = if formal && kcd2_live_pose_enabled {
        submit_camera_present(callback_generation, seq, true)
    } else {
        false
    };
    let camera_present_submitted = d3d12_camera_present_submitted || kcd2_camera_present_submitted;
    if capture_enabled && !formal && !raw_demo {
        let paired_ready = D3D12_PAIRED_WARMUP_FRAMES.load(Ordering::Acquire) >= 2;
        let observation = CaptureWarmupObservation {
            hooks_installed: true,
            hook_integrity_healthy: backend.selected_capture_safe(),
            selected_depth_ready: backend.capture_is_armed(),
            dsv_deliverable: paired_ready,
            // KCD2 validates its viewport in its separate exact-draw resolver.
            // The render-camera path must validate the actual index inputs before Ready.
            exact_draw_viewport_ready: kcd2_live_pose_enabled || d3d12_viewport_ready,
            camera: camera_warmup_readiness(callback_generation, seq),
        };
        if observe_capture_warmup(callback_generation, observation)
            && let Err(error) = arm_formal_capture(callback_generation)
        {
            record_capture_warmup_error(error);
        }
    }

    // This is the same sole SPSC consumer used by D3D11. An Arm consumed here
    // starts capture in the following render interval; it cannot claim a warmup snapshot.
    process_selected_present_feedback(seq, callback_generation);

    drop(present_run_guard);
    let original = ORIGINAL_PRESENT.load(Ordering::SeqCst);
    if original == 0 {
        return -1;
    }
    let original: unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32) -> i32 =
        unsafe { core::mem::transmute(original) };
    let perf_pre_ns = perf_frame.pre_ns();
    let result = unsafe { original(swapchain, sync_interval, flags) };
    let perf_post_started = perf_frame.start_post();

    // Original Present runs without the lifecycle lock. StopRun may finish and
    // StartRun may replace the generation before it returns; never let an old
    // callback open an interval or update timing/Ready state in that new run.
    let _post_guard = if raw_demo {
        None
    } else {
        let Some(guard) = lock_current_present_completion(
            &PRESENT_RUN_LOCK,
            &ACTIVE_GENERATION,
            callback_generation,
        ) else {
            return result;
        };
        Some(guard)
    };

    if raw_demo {
        crate::raw_depth_demo::selected_after_present(seq, result >= 0);
    }

    if should_observe_formal_present_rate(depth_capture_enabled(), formal_capture_armed()) {
        observe_present_rate();
    }

    let capture_requested = if !capture_enabled || DRAINING.load(Ordering::Acquire) {
        false
    } else if raw_demo {
        crate::raw_depth_demo::should_capture_interval(seq)
    } else if formal_capture_armed() && (present_only() || watermark_only()) {
        false
    } else if selected && formal_capture_armed() {
        backend.selected_interval_capture_requested(selected_present_accepting_candidates())
    } else if should_observe_formal_present_rate(depth_capture_enabled(), formal_capture_armed()) {
        update_cadence_for_next_frame();
        CAPTURE_THIS_FRAME.load(Ordering::Acquire)
            && CAPTURE_ISSUED.load(Ordering::Acquire) < CAPTURE_MAX_FRAMES.load(Ordering::Acquire)
    } else {
        true
    };
    if backend.capture_is_armed() {
        // Rendering after Present(N) produces frame N+1. Label the interval by
        // that future Present so its depth joins watermark/present QPC N+1.
        backend.begin_depth_capture_interval(seq.saturating_add(1), capture_requested);
    }
    if formal
        && camera_present_submitted
        && (watermark_written || present_only())
        && (paired != 0 || selected || present_only() || watermark_only())
        && result >= 0
    {
        let _ =
            FIRST_PRESENT_SEQ.compare_exchange(u64::MAX, seq, Ordering::AcqRel, Ordering::Acquire);
    }
    if paired != 0 && result < 0 {
        let error = format!(
            "D3D12 Present failed after {paired} depth frame(s) were queued: HRESULT {result:#010x}"
        );
        if raw_demo {
            crate::raw_depth_demo::record_fault(error);
        } else {
            record_capture_warmup_error(error.clone());
            if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
                latch_selected_present_fault(
                    depth_lab::selected_present_feedback::FaultReason::TransportCorrupt,
                    error,
                );
            }
        }
    }
    commit_present_perf(seq, &mut perf_frame, perf_pre_ns, perf_post_started);
    result
}

unsafe extern "system" fn hooked_present(
    swapchain: *mut core::ffi::c_void,
    sync_interval: u32,
    flags: u32,
) -> i32 {
    let mut perf_entry = PresentPerfEntry::begin(swapchain as usize);
    let Some(present_run_guard) = lock_present_observation(&PRESENT_RUN_LOCK, || {
        ACTIVE_GENERATION.load(Ordering::Acquire) == 0
            && D3D12_EARLY_BOOTSTRAP
                .get()
                .is_some_and(|bootstrap| !bootstrap.backend().runtime_observation_active())
    }) else {
        let original = ORIGINAL_PRESENT.load(Ordering::SeqCst);
        if original == 0 {
            return -1;
        }
        let original: unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32) -> i32 =
            unsafe { core::mem::transmute(original) };
        return unsafe { original(swapchain, sync_interval, flags) };
    };
    if let Some(entry) = perf_entry.as_mut() {
        entry.acquired();
    }
    let seq = PRESENT_SEQ.fetch_add(1, Ordering::Relaxed);
    // 逐帧遥测：qpc 是本帧记录的时间锚（帧时 = 相邻记录 qpc 差），pre/post 只计
    // probe 自己占用渲染线程的部分，原始 Present 的时长不混进来。
    let mut perf_frame = PresentPerfFrame::begin(perf_entry);
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let is_d3d12 = D3D12_EARLY_BOOTSTRAP
        .get()
        .is_some_and(|bootstrap| bootstrap.backend().is_d3d12_swapchain(swapchain));
    if is_d3d12 {
        return unsafe {
            hooked_present_d3d12(
                swapchain,
                sync_interval,
                flags,
                seq,
                callback_generation,
                present_run_guard,
                perf_frame,
            )
        };
    }
    if SELECTED_PRESENT_RESIZE_FAULT.swap(false, Ordering::AcqRel) {
        latch_selected_present_fault(
            depth_lab::selected_present_feedback::FaultReason::ResizeAfterReady,
            "ResizeBuffers occurred after selected-Present Ready; resource_epoch is immutable for this generation",
        );
    }
    let capture_enabled = depth_capture_enabled();
    let dsv_discovery_at_present = capture_enabled && crate::dsv_discovery::enabled();
    if capture_enabled
        && !dsv_discovery_at_present
        && callback_generation != 0
        && let Err(error) = observe_swapchain_adapter(swapchain)
    {
        record_capture_warmup_error(error.clone());
        if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::SwapchainAdapterMismatch,
                error,
            );
        }
    }
    let formal_at_present = capture_enabled && !dsv_discovery_at_present && formal_capture_armed();
    let render_evidence_at_present =
        capture_enabled && (dsv_discovery_at_present || render_evidence_frame_active());
    let mut dsv_observation_started_this_present = false;
    if formal_at_present && !FORMAL_FIRST_PRESENT_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "formal first Present: present_seq={seq} capture_this_frame={} render_evidence={} issued={} max_frames={:?}",
            CAPTURE_THIS_FRAME.load(Ordering::Acquire),
            render_evidence_at_present,
            CAPTURE_ISSUED.load(Ordering::Acquire),
            capture_max_frames()
        ));
    }
    let mut arm_after_warmup_present = false;

    if capture_enabled {
        if !game_context_hooks_installed()
            && let Err(error) = ensure_dsv_hooks_on_game_context(swapchain)
        {
            if dsv_discovery_at_present {
                crate::dsv_discovery::record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
                );
            }
            if camera_shadow_required() {
                mark_camera_hook_integrity_failure();
            }
            if !DSV_HOOK_INSTALL_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                log(&format!("B0 game context hook 安装失败: {error}"));
            }
        }

        if dsv_discovery_at_present
            && !DSV_DISCOVERY_OBSERVATION_COMPLETE.load(Ordering::Acquire)
            && game_context_hooks_installed()
        {
            match audit_dsv_discovery_hook_integrity(swapchain) {
                Ok(()) => match begin_dsv_discovery_observation_after_hook_bootstrap() {
                    Ok(started) => {
                        dsv_observation_started_this_present = started;
                        if started {
                            log(
                                "D3D11 DSV discovery observation window begins after game-context hook bootstrap; bootstrap Present excluded",
                            );
                        }
                    }
                    Err(error) => {
                        if !DSV_HOOK_INSTALL_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                            log(&error);
                        }
                    }
                },
                Err(error) => {
                    // The audit records its own exact context/vtable/lifecycle reason before
                    // returning. Do not double-count the same failed audit here.
                    if !DSV_HOOK_INSTALL_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                        log(&error);
                    }
                }
            }
        }

        if camera_shadow_required() && game_context_hooks_installed() {
            let audit_due = CAMERA_HOOK_AUDIT_DUE.swap(false, Ordering::AcqRel);
            if audit_due || seq.is_multiple_of(CAMERA_HOOK_AUDIT_INTERVAL_PRESENTS) {
                crate::perf::mark_frame(crate::perf::FrameFlag::Audit);
            }
            if (audit_due || seq.is_multiple_of(CAMERA_HOOK_AUDIT_INTERVAL_PRESENTS))
                && let Err(error) = audit_camera_hook_integrity_from_swapchain(swapchain, audit_due)
            {
                mark_camera_hook_integrity_failure();
                if !CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                    log(&format!(
                        "camera hook integrity audit failed before Present: {error}"
                    ));
                }
            }
        }

        let dsv_selection = if dsv_discovery_at_present {
            if DSV_DISCOVERY_OBSERVATION_STARTED.load(Ordering::Acquire)
                && !dsv_observation_started_this_present
            {
                // Keep frame finalization, report admission, and exact-limit closure in one
                // lifecycle epoch. Stop cannot seal between tracker drain and observe_frame.
                if let Some(entry) = crate::dsv_discovery::try_enter_observation()
                    && let Some(frame) = finish_dsv_discovery_frame(&entry, seq)
                {
                    entry.observe_frame(frame);
                    close_dsv_discovery_observation_at_frame_limit(&entry);
                }
            }
            None
        } else {
            render_evidence_at_present
                .then(finish_dsv_selection_audit)
                .flatten()
        };
        // 仅水印档到此为止：水印已在上面写过（开销完整保留），回读整条跳过。
        // 判断放在这里而不是 formal_at_present 里——后者还管着水印，
        // 混进去会让「仅水印」退化成「什么都不做」。
        if formal_at_present && !watermark_only() {
            if let Err(error) =
                capture_last_completed_at_present(seq, callback_generation, dsv_selection)
            {
                // Camera production can fail asynchronously after the previous Present. In that
                // case exact DSV bookkeeping is intentionally disabled, so the next missing
                // snapshot is a consequence, not a ring identity fault. Preserve the Camera root
                // cause for the control thread instead of letting this secondary error replace it.
                let camera_failure = camera_runtime_failure();
                if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) && camera_failure.is_none() {
                    latch_selected_present_fault(
                        depth_lab::selected_present_feedback::FaultReason::RingIdentityMismatch,
                        format!("candidate materialization failed for present_seq {seq}: {error}"),
                    );
                }
                if exact_production_snapshot_enabled() {
                    crate::camera_probe::fail_depth_projection(&error);
                }
                if !CAPTURE_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                    log(&format!("P1c Present 前复制最终 DSV 快照失败: {error}"));
                }
            }

            if let Err(error) = drain_ready_and_queue(swapchain, seq, callback_generation) {
                if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
                    latch_selected_present_fault(
                        depth_lab::selected_present_feedback::FaultReason::ReadbackMapFailed,
                        format!("selected depth drain failed: {error}"),
                    );
                }
                if !DRAIN_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                    log(&format!("P1c 回读/排队失败: {error}"));
                }
            }

            if seq > 0
                && seq.is_multiple_of(300)
                && let Some(stats) = delivery_stats()
            {
                log(&format!(
                    "P1c stats present_seq={seq} pool_full={} timed_out={} writer_queue_full={} writer_bypassed={} delivery_rate={:.4}",
                    stats.readback.drops.pool_full,
                    stats.readback.drops.timed_out,
                    stats.writer_queue_full,
                    stats.writer_bypassed,
                    stats.delivery_rate(),
                ));
            }
        } else if !dsv_discovery_at_present {
            // The main DSV is normally unbound before Present, so CURRENT_SELECTED_DEPTH being
            // empty here is healthy and expected. Readiness needs the persistent, uniquely audited
            // resource identity; requiring the transient OM binding makes Ready impossible.
            let selected_depth_ready = DEPTH_SELECTION
                .try_lock()
                .is_ok_and(|selection| selection.has_unique_resource());
            let dsv_ready = dsv_selection
                .as_ref()
                .is_some_and(depth_lab::DsvSelectionAudit::is_deliverable);
            let observation = CaptureWarmupObservation {
                hooks_installed: game_context_hooks_installed(),
                hook_integrity_healthy: !CAMERA_HOOK_INTEGRITY_FAILED.load(Ordering::Acquire),
                selected_depth_ready,
                dsv_deliverable: dsv_ready,
                exact_draw_viewport_ready: exact_draw_viewport_ready(callback_generation, seq),
                camera: camera_warmup_readiness(callback_generation, seq),
            };
            arm_after_warmup_present = observe_capture_warmup(callback_generation, observation);
        }
    }

    if DRAINING.load(Ordering::Acquire)
        && let Err(error) = drain_ready_and_queue(swapchain, seq, callback_generation)
    {
        if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::ReadbackMapFailed,
                format!("selected depth drain after StopRun failed: {error}"),
            );
        }
        if !DRAIN_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
            log(&format!("P1c drain after StopRun failed: {error}"));
        }
    }

    let camera_present_submitted = if formal_at_present {
        let _perf = crate::perf::scope(crate::perf::Section::Pose);
        submit_camera_present(callback_generation, seq, render_evidence_at_present)
    } else {
        false
    };
    if arm_after_warmup_present && let Err(error) = arm_formal_capture(callback_generation) {
        record_capture_warmup_error(error);
    }

    process_selected_present_feedback(seq, callback_generation);

    let watermark_written = if formal_at_present && !present_only() {
        // 水印必须在真正的 Present **之前**写进后缓冲（§5.6）。配置关闭后也必须停止；
        // 否则“关掉深度采集”仍会继续修改游戏画面。
        let _perf = crate::perf::scope(crate::perf::Section::Watermark);
        match crate::watermark_gpu::write(
            swapchain,
            seq as u16,
            WATERMARK_ORIGIN.0,
            WATERMARK_ORIGIN.1,
        ) {
            Ok(()) => true,
            Err(e) => {
                if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
                    latch_selected_present_fault(
                        depth_lab::selected_present_feedback::FaultReason::WatermarkWriteFailed,
                        format!("watermark write failed for present_seq {seq}: {e}"),
                    );
                }
                // 只记一次，不要每帧刷日志把磁盘写满
                if seq.is_multiple_of(3600) {
                    log(&format!("水印写入失败（第 {seq} 帧）: {e}"));
                }
                false
            }
        }
    } else {
        false
    };

    if seq > 0
        && seq.is_multiple_of(600)
        && let Some(summary) = crate::perf::take_summary()
    {
        log(&format!("depth perf aggregate present_seq={seq} {summary}"));
    }

    drop(present_run_guard);

    let original = ORIGINAL_PRESENT.load(Ordering::SeqCst);
    if original == 0 {
        return 0;
    }
    let backing_before_present = if camera_shadow_required()
        && game_context_hooks_installed()
        && !BACKING_VTABLE_PRESENT_TRANSITION_LOGGED.load(Ordering::Acquire)
    {
        backing_hook_snapshot().ok()
    } else {
        None
    };
    let original: unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32) -> i32 =
        unsafe { core::mem::transmute(original) };
    let perf_pre_ns = perf_frame.pre_ns();
    let result = unsafe { original(swapchain, sync_interval, flags) };
    let perf_post_started = perf_frame.start_post();
    if should_publish_first_present(
        camera_present_submitted,
        watermark_written,
        present_only(),
        result,
    ) {
        let _ =
            FIRST_PRESENT_SEQ.compare_exchange(u64::MAX, seq, Ordering::AcqRel, Ordering::Acquire);
    }

    if let Some(before) = backing_before_present
        && let Ok(after) = backing_hook_snapshot()
        && !BACKING_VTABLE_PRESENT_TRANSITION_LOGGED.swap(true, Ordering::AcqRel)
    {
        log(&format!(
            "camera backing vtable around original Present status={} before={before:?} after={after:?}",
            if backing_draws_restored_inside_present(before, after) {
                "draw_slots_restored_inside_original_present"
            } else {
                "no_full_draw_restore_transition"
            }
        ));
    }
    // 每帧只做一次 owner 指针和当前模式受保护 shadow 槽的原子读取。健康路径没有
    // VirtualProtect/写操作；若 original Present 真改了 owner/slot，返回游戏前立刻
    // 按严格身份恢复，因此这里没有合法 immediate-context callback 可落进缺口。
    maintain_dsv_discovery_shadow_after_original_present();
    maintain_camera_shadow_after_original_present();

    // 真正的 Present 可能因 VSync 阻塞。必须等它返回后再读 QPC：这才是下一帧
    // 即将开始的时刻。若在调用前判定，节拍器拿到的是一段阻塞之前的旧时间，
    // 60/30 fps 车道上会把 Present 等待误算进下一帧，造成相位抖动甚至漏采。
    // 此刻本帧的两个判定点也都已经读完，共享标志可以安全写成下一帧的结论。
    if should_observe_formal_present_rate(depth_capture_enabled(), formal_capture_armed()) {
        update_cadence_for_next_frame();
        observe_present_rate();
    }

    commit_present_perf(seq, &mut perf_frame, perf_pre_ns, perf_post_started);

    result
}

const fn should_observe_formal_present_rate(
    depth_capture_enabled: bool,
    formal_capture_armed: bool,
) -> bool {
    depth_capture_enabled && formal_capture_armed
}

fn commit_present_perf(
    seq: u64,
    perf_frame: &mut PresentPerfFrame,
    perf_pre_ns: u64,
    perf_post_started: Option<std::time::Instant>,
) {
    observe_local_compatibility();
    if let Some(post_started) = perf_post_started {
        let post_ns = u64::try_from(post_started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        let timing = perf_frame.begin_commit();
        if let Some(commit) =
            crate::perf::commit_frame(seq, perf_frame.frame_qpc, perf_pre_ns, post_ns, timing)
        {
            perf_frame.committed(commit);
        }
    }
}

const fn should_publish_first_present(
    camera_present_submitted: bool,
    watermark_written: bool,
    present_only: bool,
    present_hresult: i32,
) -> bool {
    camera_present_submitted && (present_only || watermark_written) && present_hresult >= 0
}

fn observe_swapchain_adapter(swapchain: *mut core::ffi::c_void) -> Result<(), String> {
    if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get()
        && bootstrap.backend().is_d3d12_swapchain(swapchain)
    {
        let device = bootstrap.backend().swapchain_device(swapchain)?;
        let device_key = device.as_raw() as usize;
        let accepted_device_key = OBSERVED_SWAPCHAIN_DEVICE_PTR.load(Ordering::Acquire);
        if accepted_device_key == device_key {
            return Ok(());
        }
        if accepted_device_key != 0 {
            return Err(format!(
                "active generation D3D12 device changed from {accepted_device_key:#x} to {device_key:#x}; resource_epoch is immutable"
            ));
        }
        let observed = query_d3d12_adapter_identity(&device)?;
        let mut current = ACTUAL_SWAPCHAIN_ADAPTER
            .lock()
            .map_err(|_| "swapchain adapter identity lock poisoned".to_string())?;
        if let Some(expected) = current.as_ref()
            && expected != &observed
        {
            return Err(format!(
                "active generation observed D3D12 swapchains on different adapters: first={expected:?}, current={observed:?}"
            ));
        }
        if current.is_none() {
            log(&format!(
                "actual D3D12 swapchain adapter: {} driver={} luid={}:{}",
                observed.description,
                observed.driver_version,
                observed.luid_high,
                observed.luid_low
            ));
            *current = Some(observed);
        }
        *OBSERVED_D3D12_DEVICE
            .lock()
            .map_err(|_| "D3D12 swapchain device identity lock poisoned".to_string())? =
            Some(device);
        OBSERVED_SWAPCHAIN_DEVICE_PTR.store(device_key, Ordering::Release);
        return Ok(());
    }
    let sc = unsafe { IDXGISwapChain::from_raw_borrowed(&swapchain) }
        .ok_or("actual game swapchain pointer is invalid")?;
    // One GetDevice per Present is intentional: a bare swapchain address can
    // be ABA-reused after object destruction. The returned device is compared
    // against strong references retained by this generation, so pointer reuse
    // cannot bypass adapter validation.
    let device: ID3D11Device = unsafe { sc.GetDevice() }.map_err(|error| {
        format!("actual game swapchain GetDevice<ID3D11Device> failed: {error}")
    })?;
    let device_key = device.as_raw() as usize;
    let accepted_device_key = OBSERVED_SWAPCHAIN_DEVICE_PTR.load(Ordering::Acquire);
    if accepted_device_key == device_key {
        return Ok(());
    }
    if accepted_device_key != 0 {
        return Err(format!(
            "active generation swapchain D3D device changed from {accepted_device_key:#x} to {device_key:#x}; resource_epoch is immutable"
        ));
    }

    let observed = query_swapchain_adapter_identity(&device)?;
    let mut current = ACTUAL_SWAPCHAIN_ADAPTER
        .lock()
        .map_err(|_| "swapchain adapter identity lock poisoned".to_string())?;
    if let Some(expected) = current.as_ref()
        && expected != &observed
    {
        return Err(format!(
            "active generation observed swapchains on different adapters: first={expected:?}, current={observed:?}"
        ));
    }
    if current.is_none() {
        log(&format!(
            "actual game swapchain adapter: {} driver={} luid={}:{}",
            observed.description, observed.driver_version, observed.luid_high, observed.luid_low
        ));
        *current = Some(observed);
    }
    *OBSERVED_SWAPCHAIN_DEVICE
        .lock()
        .map_err(|_| "swapchain device identity lock poisoned".to_string())? = Some(device);
    OBSERVED_SWAPCHAIN_DEVICE_PTR.store(device_key, Ordering::Release);
    Ok(())
}

fn query_d3d12_adapter_identity(
    device: &ID3D12Device,
) -> Result<depth_lab::GpuAdapterIdentity, String> {
    let luid = unsafe { device.GetAdapterLuid() };
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }
        .map_err(|error| format!("create DXGI factory for D3D12 adapter query failed: {error}"))?;
    let adapter: IDXGIAdapter1 = unsafe { factory.EnumAdapterByLuid(luid) }
        .map_err(|error| format!("D3D12 adapter LUID lookup failed: {error}"))?;
    let description = unsafe { adapter.GetDesc1() }
        .map_err(|error| format!("D3D12 adapter GetDesc1 failed: {error}"))?;
    let raw_driver_version =
        unsafe { adapter.CheckInterfaceSupport(&dxgi_driver_version_query_iid()) }
            .map_err(|error| format!("D3D12 adapter driver version query failed: {error}"))?;
    Ok(depth_lab::GpuAdapterIdentity {
        description: utf16_z(&description.Description),
        driver_version: format_driver_version(raw_driver_version),
        luid_high: description.AdapterLuid.HighPart,
        luid_low: description.AdapterLuid.LowPart,
    })
}

fn query_swapchain_adapter_identity(
    device: &ID3D11Device,
) -> Result<depth_lab::GpuAdapterIdentity, String> {
    let dxgi_device: IDXGIDevice = device
        .cast()
        .map_err(|error| format!("actual game device is not IDXGIDevice: {error}"))?;
    let adapter = unsafe { dxgi_device.GetAdapter() }
        .map_err(|error| format!("actual game device GetAdapter failed: {error}"))?;
    let adapter: IDXGIAdapter1 = adapter
        .cast()
        .map_err(|error| format!("actual game adapter is not IDXGIAdapter1: {error}"))?;
    let description = unsafe { adapter.GetDesc1() }
        .map_err(|error| format!("actual game adapter GetDesc1 failed: {error}"))?;
    let raw_driver_version =
        unsafe { adapter.CheckInterfaceSupport(&dxgi_driver_version_query_iid()) }
            .map_err(|error| format!("actual game adapter driver version query failed: {error}"))?;
    Ok(depth_lab::GpuAdapterIdentity {
        description: utf16_z(&description.Description),
        driver_version: format_driver_version(raw_driver_version),
        luid_high: description.AdapterLuid.HighPart,
        luid_low: description.AdapterLuid.LowPart,
    })
}

/// Drop all adapter/device observations after the render producer has been
/// quiesced. Atomic keys are cleared before the strong COM owners so no fast
/// path can accept a pointer after its ABA protection has been released.
fn clear_observed_swapchain_devices() -> Result<(), String> {
    OBSERVED_SWAPCHAIN_DEVICE_PTR.store(0, Ordering::Release);
    let mut errors = Vec::new();
    match ACTUAL_SWAPCHAIN_ADAPTER.lock() {
        Ok(mut identity) => *identity = None,
        Err(poisoned) => {
            *poisoned.into_inner() = None;
            errors.push("swapchain adapter identity lock was poisoned during cleanup");
        }
    }
    match OBSERVED_SWAPCHAIN_DEVICE.lock() {
        Ok(mut device) => *device = None,
        Err(poisoned) => {
            *poisoned.into_inner() = None;
            errors.push("swapchain device identity lock was poisoned during cleanup");
        }
    }
    match OBSERVED_D3D12_DEVICE.lock() {
        Ok(mut device) => *device = None,
        Err(poisoned) => {
            *poisoned.into_inner() = None;
            errors.push("D3D12 swapchain device identity lock was poisoned during cleanup");
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn dxgi_driver_version_query_iid() -> windows::core::GUID {
    // IDXGIAdapter::CheckInterfaceSupport reports the DXGI driver version for
    // the adapter. ID3D12Device is not a valid query interface on every driver
    // and can return DXGI_ERROR_UNSUPPORTED even for the active D3D12 device.
    IDXGIDevice::IID
}

fn format_driver_version(raw: i64) -> String {
    let bits = raw as u64;
    let high = (bits >> 32) as u32;
    let low = bits as u32;
    format!(
        "{}.{}.{}.{}",
        high >> 16,
        high & 0xffff,
        low >> 16,
        low & 0xffff
    )
}

fn utf16_z(raw: &[u16]) -> String {
    let len = raw
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

fn draw_hooks_installed() -> bool {
    DRAW_HOOK.get().is_some()
        && DRAW_INDEXED_HOOK.get().is_some()
        && DRAW_INDEXED_INSTANCED_HOOK.get().is_some()
        && DRAW_INSTANCED_HOOK.get().is_some()
        && DRAW_AUTO_HOOK.get().is_some()
        && DRAW_INDEXED_INSTANCED_INDIRECT_HOOK.get().is_some()
        && DRAW_INSTANCED_INDIRECT_HOOK.get().is_some()
}

fn camera_buffer_hooks_installed() -> bool {
    MAP_HOOK.get().is_some()
        && UNMAP_HOOK.get().is_some()
        && UPDATE_SUBRESOURCE_HOOK.get().is_some()
        && VS_SET_CONSTANT_BUFFERS_HOOK.get().is_some()
}

fn game_context_hooks_installed() -> bool {
    (!game_context_shadow_required() || GAME_CONTEXT_VTABLE_SHADOW.get().is_some())
        && OM_SET_RENDER_TARGETS_HOOK.get().is_some()
        && OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK
            .get()
            .is_some()
        && CLEAR_DEPTH_STENCIL_VIEW_HOOK.get().is_some()
        && CLEAR_STATE_HOOK.get().is_some()
        && EXECUTE_COMMAND_LIST_HOOK.get().is_some()
        && draw_hooks_installed()
        && (!crate::dsv_discovery::enabled() || DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire))
        && (!crate::dsv_discovery::enabled()
            || (SET_MULTITHREAD_PROTECTED_HOOK.get().is_some()
                && GAME_MULTITHREAD_CONTEXT.get().is_some()))
        && (!crate::camera_probe::enabled() || camera_buffer_hooks_installed())
}

const fn camera_capture_requires_shadow(discovery_enabled: bool, production_enabled: bool) -> bool {
    discovery_enabled || production_enabled
}

fn camera_shadow_required() -> bool {
    camera_capture_requires_shadow(
        crate::camera_probe::discovery_enabled(),
        crate::camera_probe::production_enabled(),
    )
}

const fn depth_observation_requires_shadow(
    camera_discovery_enabled: bool,
    camera_production_enabled: bool,
    dsv_discovery_enabled: bool,
) -> bool {
    camera_capture_requires_shadow(camera_discovery_enabled, camera_production_enabled)
        || dsv_discovery_enabled
}

fn game_context_shadow_required() -> bool {
    depth_observation_requires_shadow(
        crate::camera_probe::discovery_enabled(),
        crate::camera_probe::production_enabled(),
        crate::dsv_discovery::enabled(),
    )
}

fn mark_camera_hook_integrity_failure() {
    CAMERA_HOOK_INTEGRITY_FAILED.store(true, Ordering::Release);
    crate::camera_probe::mark_production_hook_integrity_failure(
        "Camera hook integrity failed; production callbacks may be incomplete",
    );
    if !CAMERA_HOOK_INTEGRITY_MARKED.swap(true, Ordering::AcqRel) {
        crate::camera_probe::mark_runtime_callback_dropped("camera hook integrity failure");
    }
}

fn mark_camera_hook_callback_gap() {
    crate::camera_probe::mark_production_hook_integrity_failure(
        "Camera hook callback gap detected; production sample continuity is incomplete",
    );
    if !CAMERA_HOOK_INTEGRITY_MARKED.swap(true, Ordering::AcqRel) {
        crate::camera_probe::mark_runtime_callback_dropped(
            "camera hook integrity audit found a possible callback gap",
        );
    }
}

fn camera_integrity_hooks() -> Result<[(&'static str, usize, &'static VtableHook); 9], String> {
    Ok([
        (
            "draw_indexed",
            d3d11_context::DRAW_INDEXED,
            DRAW_INDEXED_HOOK.get().ok_or("DrawIndexed hook missing")?,
        ),
        (
            "draw",
            d3d11_context::DRAW,
            DRAW_HOOK.get().ok_or("Draw hook missing")?,
        ),
        (
            "draw_indexed_instanced",
            d3d11_context::DRAW_INDEXED_INSTANCED,
            DRAW_INDEXED_INSTANCED_HOOK
                .get()
                .ok_or("DrawIndexedInstanced hook missing")?,
        ),
        (
            "draw_instanced",
            d3d11_context::DRAW_INSTANCED,
            DRAW_INSTANCED_HOOK
                .get()
                .ok_or("DrawInstanced hook missing")?,
        ),
        (
            "draw_auto",
            d3d11_context::DRAW_AUTO,
            DRAW_AUTO_HOOK.get().ok_or("DrawAuto hook missing")?,
        ),
        (
            "draw_indexed_instanced_indirect",
            d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT,
            DRAW_INDEXED_INSTANCED_INDIRECT_HOOK
                .get()
                .ok_or("DrawIndexedInstancedIndirect hook missing")?,
        ),
        (
            "draw_instanced_indirect",
            d3d11_context::DRAW_INSTANCED_INDIRECT,
            DRAW_INSTANCED_INDIRECT_HOOK
                .get()
                .ok_or("DrawInstancedIndirect hook missing")?,
        ),
        (
            "om_set_render_targets",
            d3d11_context::OM_SET_RENDER_TARGETS,
            OM_SET_RENDER_TARGETS_HOOK
                .get()
                .ok_or("OMSetRenderTargets hook missing")?,
        ),
        (
            "om_set_render_targets_and_unordered_access_views",
            d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
            OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK
                .get()
                .ok_or("OMSetRenderTargetsAndUnorderedAccessViews hook missing")?,
        ),
    ])
}

fn dsv_discovery_integrity_hooks() -> Result<Vec<(&'static str, usize, &'static VtableHook)>, String>
{
    let mut hooks = camera_integrity_hooks()?.to_vec();
    hooks.push((
        "clear_depth_stencil_view",
        d3d11_context::CLEAR_DEPTH_STENCIL_VIEW,
        CLEAR_DEPTH_STENCIL_VIEW_HOOK
            .get()
            .ok_or("ClearDepthStencilView hook missing")?,
    ));
    hooks.push((
        "clear_state",
        d3d11_context::CLEAR_STATE,
        CLEAR_STATE_HOOK.get().ok_or("ClearState hook missing")?,
    ));
    hooks.push((
        "execute_command_list",
        d3d11_context::EXECUTE_COMMAND_LIST,
        EXECUTE_COMMAND_LIST_HOOK
            .get()
            .ok_or("ExecuteCommandList hook missing")?,
    ));
    if let Some(hook) = SWAP_DEVICE_CONTEXT_STATE_HOOK.get() {
        hooks.push((
            "swap_device_context_state",
            d3d11_context::SWAP_DEVICE_CONTEXT_STATE,
            hook,
        ));
    }
    if let Some(hook) = CLEAR_VIEW_HOOK.get() {
        hooks.push(("clear_view", d3d11_context::CLEAR_VIEW, hook));
    }
    Ok(hooks)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct BackingHookSnapshot {
    draw_original: u8,
    draw_replacement: u8,
    draw_unknown: u8,
    om_original: u8,
    om_replacement: u8,
    om_unknown: u8,
}

fn record_backing_target(
    snapshot: &mut BackingHookSnapshot,
    is_draw: bool,
    current: usize,
    original: usize,
    replacement: usize,
) {
    let counts = if current == original {
        if is_draw {
            &mut snapshot.draw_original
        } else {
            &mut snapshot.om_original
        }
    } else if current == replacement {
        if is_draw {
            &mut snapshot.draw_replacement
        } else {
            &mut snapshot.om_replacement
        }
    } else if is_draw {
        &mut snapshot.draw_unknown
    } else {
        &mut snapshot.om_unknown
    };
    *counts += 1;
}

fn backing_hook_snapshot() -> Result<BackingHookSnapshot, String> {
    let shadow = GAME_CONTEXT_VTABLE_SHADOW
        .get()
        .ok_or("game context shadow vtable missing")?;
    let mut snapshot = BackingHookSnapshot::default();
    for (ordinal, (_, index, hook)) in camera_integrity_hooks()?.into_iter().enumerate() {
        record_backing_target(
            &mut snapshot,
            ordinal < CameraDrawMethod::ALL.len(),
            shadow.backing_target(index),
            hook.original(),
            hook.replacement(),
        );
    }
    Ok(snapshot)
}

fn backing_draws_restored_inside_present(
    before: BackingHookSnapshot,
    after: BackingHookSnapshot,
) -> bool {
    before.draw_replacement == CameraDrawMethod::ALL.len() as u8
        && before.draw_original == 0
        && before.draw_unknown == 0
        && after.draw_original == CameraDrawMethod::ALL.len() as u8
        && after.draw_replacement == 0
        && after.draw_unknown == 0
}

fn camera_shadow_fast_path_healthy() -> bool {
    let Some(shadow) = GAME_CONTEXT_VTABLE_SHADOW.get() else {
        return false;
    };
    if shadow.owner_state() != VtableOwnerState::Active {
        return false;
    }
    camera_integrity_hooks().is_ok_and(|hooks| {
        hooks.into_iter().all(|(_, index, hook)| {
            shadow.slot_state(index, hook.original(), hook.replacement())
                == VtableSlotState::Installed
        })
    })
}

fn dsv_discovery_shadow_fast_path_healthy() -> bool {
    let Some(shadow) = GAME_CONTEXT_VTABLE_SHADOW.get() else {
        return false;
    };
    if shadow.owner_state() != VtableOwnerState::Active {
        return false;
    }
    dsv_discovery_integrity_hooks().is_ok_and(|hooks| {
        hooks.into_iter().all(|(_, index, hook)| {
            shadow.slot_state(index, hook.original(), hook.replacement())
                == VtableSlotState::Installed
        })
    })
}

fn deferred_dsv_hooks_fast_path_healthy() -> bool {
    let Some(context) = DEFERRED_DUMMY_CONTEXT.get() else {
        return false;
    };
    DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire)
        && deferred_dsv_integrity_hooks().is_ok_and(|hooks| {
            hooks.into_iter().all(|(_, index, hook)| {
                (unsafe { hook.state(context.as_raw(), index) }) == VtableSlotState::Installed
            })
        })
}

fn maintain_dsv_discovery_shadow_after_original_present() {
    if !crate::dsv_discovery::enabled()
        || DSV_DISCOVERY_OBSERVATION_COMPLETE.load(Ordering::Acquire)
        || !game_context_hooks_installed()
        || (dsv_discovery_shadow_fast_path_healthy() && deferred_dsv_hooks_fast_path_healthy())
    {
        return;
    }
    let context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
    let result = if context == 0 {
        Err("post-Present DSV discovery shadow audit has no game immediate context".into())
    } else {
        audit_dsv_discovery_hook_integrity_on_context(context as *mut core::ffi::c_void, false)
            .and_then(|_| audit_multithread_protection_hook(true))
            .and_then(|_| audit_deferred_dsv_hook_integrity(true))
    };
    if let Err(error) = result
        && !DSV_HOOK_INSTALL_ERROR_LOGGED.swap(true, Ordering::AcqRel)
    {
        log(&format!(
            "D3D11 DSV discovery shadow continuity failed after original Present: {error}"
        ));
    }
}

fn maintain_camera_shadow_after_original_present() {
    if !camera_shadow_required()
        || !game_context_hooks_installed()
        || camera_shadow_fast_path_healthy()
    {
        return;
    }
    let context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
    let verbose = !POST_PRESENT_SHADOW_REPAIR_LOGGED.swap(true, Ordering::AcqRel);
    let result = if context == 0 {
        Err("post-Present shadow audit has no game immediate context".into())
    } else {
        audit_camera_hook_integrity(
            context as *mut core::ffi::c_void,
            "after_original_present",
            verbose,
            false,
        )
    };
    if let Err(error) = result {
        mark_camera_hook_integrity_failure();
        if !CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
            log(&format!(
                "camera shadow continuity failed after original Present: {error}"
            ));
        }
    }
}

fn audit_camera_hook_integrity(
    actual_context: *mut core::ffi::c_void,
    source: &str,
    verbose: bool,
    callback_gap_possible: bool,
) -> Result<usize, String> {
    let _guard = CAMERA_HOOK_INTEGRITY_AUDIT_LOCK
        .lock()
        .map_err(|_| "camera hook integrity audit lock poisoned".to_string())?;
    let shadow = GAME_CONTEXT_VTABLE_SHADOW
        .get()
        .ok_or("game context shadow vtable missing")?;
    let expected_context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
    let actual_vtable = context_vtable_address(actual_context);
    let expected_vtable = shadow.shadow_vtable_address();
    if expected_context == 0 || expected_context != actual_context as usize {
        let details = format!(
            "source={source} status=context_changed expected_context={expected_context:#x} expected_vtable={expected_vtable:#x} actual_context={:#x} actual_vtable={actual_vtable:#x}",
            actual_context as usize,
        );
        mark_camera_hook_integrity_failure();
        let first_error = !CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.swap(true, Ordering::AcqRel);
        if verbose || first_error {
            log(&format!("camera hook integrity audit {details}"));
        }
        return Err(details);
    }

    let hooks = camera_integrity_hooks()?;

    let mut slots = Vec::with_capacity(hooks.len());
    let mut failures = Vec::new();
    let mut rearmed = 0usize;
    let mut restored_seen = false;
    let mut owner_rearmed = false;
    match shadow.owner_state() {
        VtableOwnerState::Active => {}
        VtableOwnerState::RestoredOriginal => {
            restored_seen = true;
            match unsafe { shadow.rearm_if_restored() } {
                Ok(true) => owner_rearmed = true,
                Ok(false) => {}
                Err(error) => failures.push(format!("context_vtable_owner: {error}")),
            }
        }
        VtableOwnerState::ReplacedUnknown { current } => failures.push(format!(
            "context_vtable_owner: unknown_target backing={:#x} shadow={:#x} current={current:#x}",
            shadow.backing_vtable_address(),
            shadow.shadow_vtable_address(),
        )),
    }
    for (label, index, hook) in hooks {
        let slot = shadow.shadow_vtable_address() + index * core::mem::size_of::<usize>();
        let state = shadow.slot_state(index, hook.original(), hook.replacement());
        match state {
            VtableSlotState::Installed => slots.push(format!(
                "{label}=installed(slot={:#x},target={:#x})",
                slot,
                hook.replacement()
            )),
            VtableSlotState::RestoredOriginal => {
                restored_seen = true;
                match shadow.rearm_slot_if_restored(
                    index,
                    hook.original(),
                    hook.replacement(),
                ) {
                    Ok(true) => {
                        rearmed += 1;
                        slots.push(format!(
                            "{label}=rearmed(slot={:#x},original={:#x},target={:#x})",
                            slot,
                            hook.original(),
                            hook.replacement()
                        ));
                    }
                    Ok(false) => slots.push(format!(
                        "{label}=installed_after_race(slot={:#x},target={:#x})",
                        slot,
                        hook.replacement()
                    )),
                    Err(error) => failures.push(format!("{label}: {error}")),
                }
            }
            VtableSlotState::ReplacedUnknown { current } => failures.push(format!(
                "{label}: unknown_target slot={:#x} original={:#x} replacement={:#x} current={current:#x}",
                slot,
                hook.original(),
                hook.replacement(),
            )),
            VtableSlotState::DifferentSlot {
                expected_slot,
                actual_slot,
                current,
            } => failures.push(format!(
                "{label}: vtable_slot_changed expected_slot={expected_slot:#x} actual_slot={actual_slot:#x} current={current:#x}"
            )),
        }
    }

    if !failures.is_empty() {
        let details = format!(
            "source={source} status=failed context={:#x} observed_vtable={actual_vtable:#x} backing_vtable={:#x} shadow_vtable={:#x} failures=[{}] slots=[{}]",
            actual_context as usize,
            shadow.backing_vtable_address(),
            shadow.shadow_vtable_address(),
            failures.join("; "),
            slots.join(" "),
        );
        mark_camera_hook_integrity_failure();
        let first_error = !CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.swap(true, Ordering::AcqRel);
        if verbose || first_error {
            log(&format!("camera hook integrity audit {details}"));
        }
        return Err(details);
    }

    let recovered = CAMERA_HOOK_INTEGRITY_FAILED.swap(false, Ordering::AcqRel);
    CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.store(false, Ordering::Release);
    if restored_seen && callback_gap_possible {
        // 审计间隔内可能已经漏过回调；即使入口恢复，本 phase 仍必须不可靠。
        mark_camera_hook_callback_gap();
    }
    if verbose || recovered || (rearmed != 0 && callback_gap_possible) {
        log(&format!(
            "camera hook integrity audit source={source} status={} context={:#x} observed_vtable={actual_vtable:#x} backing_vtable={:#x} shadow_vtable={:#x} owner_rearmed={owner_rearmed} slot_rearmed={rearmed} callback_gap_possible={callback_gap_possible} slots=[{}]",
            if recovered { "recovered" } else { "healthy" },
            actual_context as usize,
            shadow.backing_vtable_address(),
            shadow.shadow_vtable_address(),
            slots.join(" "),
        ));
    }
    Ok(rearmed + usize::from(owner_rearmed))
}

fn audit_camera_hook_integrity_from_swapchain(
    swapchain: *mut core::ffi::c_void,
    verbose: bool,
) -> Result<usize, String> {
    let sc = unsafe { IDXGISwapChain::from_raw_borrowed(&swapchain) }
        .ok_or("hook integrity audit swapchain pointer invalid")?;
    let device: ID3D11Device = unsafe { sc.GetDevice() }
        .map_err(|error| format!("hook integrity audit GetDevice failed: {error}"))?;
    let context: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
        .map_err(|error| format!("hook integrity audit GetImmediateContext failed: {error}"))?;
    audit_camera_hook_integrity(
        context.as_raw(),
        "swapchain_immediate_context",
        verbose,
        true,
    )
}

fn audit_camera_hook_integrity_before_phase_reset() -> Result<(), String> {
    if !game_context_hooks_installed() {
        CAMERA_HOOK_AUDIT_DUE.store(true, Ordering::Release);
        return Ok(());
    }
    let context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
    if context == 0 {
        return Err("camera hook handles exist without a game immediate context".into());
    }
    audit_camera_hook_integrity(
        context as *mut core::ffi::c_void,
        "phase_reset_stored_context",
        true,
        true,
    )?;
    Ok(())
}

fn context_shadow_vtable_shape(context: &ID3D11DeviceContext) -> (usize, &'static str) {
    let base = context.as_raw();
    if let Ok(value) = context.cast::<ID3D11DeviceContext4>()
        && value.as_raw() == base
    {
        return (D3D11_DEVICE_CONTEXT4_VTABLE_ENTRIES, "ID3D11DeviceContext4");
    }
    if let Ok(value) = context.cast::<ID3D11DeviceContext3>()
        && value.as_raw() == base
    {
        return (D3D11_DEVICE_CONTEXT3_VTABLE_ENTRIES, "ID3D11DeviceContext3");
    }
    if let Ok(value) = context.cast::<ID3D11DeviceContext2>()
        && value.as_raw() == base
    {
        return (D3D11_DEVICE_CONTEXT2_VTABLE_ENTRIES, "ID3D11DeviceContext2");
    }
    if let Ok(value) = context.cast::<ID3D11DeviceContext1>()
        && value.as_raw() == base
    {
        return (D3D11_DEVICE_CONTEXT1_VTABLE_ENTRIES, "ID3D11DeviceContext1");
    }
    (D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES, "ID3D11DeviceContext")
}

/// `ID3D11DeviceContext1..4` inherit the complete base-context ABI, but COM permits an
/// implementation to return a tear-off interface with a different pointer/vtable. The hooks in
/// this module patch one implementation table, so accepting such an alias would let inherited
/// OM/Draw/Execute/Finish calls bypass the ledger. Native D3D11 uses one identity; anything else
/// is rejected rather than guessed.
fn verify_context_inherited_aliases(
    context: &ID3D11DeviceContext,
    label: &str,
) -> Result<bool, String> {
    let base_raw = context.as_raw();
    let base_vtable = context_vtable_address(base_raw);
    let mut context1_supported = false;
    let mut higher_supported = false;
    macro_rules! verify_alias {
        ($interface:ty, $name:literal, $is_context1:expr) => {
            if let Ok(alias) = context.cast::<$interface>() {
                if alias.as_raw() != base_raw
                    || context_vtable_address(alias.as_raw()) != base_vtable
                {
                    return Err(format!(
                        "{label} {} is a tear-off alias; inherited DSV hook coverage cannot be proven",
                        $name
                    ));
                }
                if $is_context1 {
                    context1_supported = true;
                } else {
                    higher_supported = true;
                }
            }
        };
    }
    verify_alias!(ID3D11DeviceContext1, "ID3D11DeviceContext1", true);
    verify_alias!(ID3D11DeviceContext2, "ID3D11DeviceContext2", false);
    verify_alias!(ID3D11DeviceContext3, "ID3D11DeviceContext3", false);
    verify_alias!(ID3D11DeviceContext4, "ID3D11DeviceContext4", false);
    if higher_supported && !context1_supported {
        return Err(format!(
            "{label} exposes a higher context interface without the inherited Context1 ABI"
        ));
    }
    Ok(context1_supported)
}

fn verify_immediate_context_is_runtime_unprotected(
    context: &ID3D11DeviceContext,
    label: &str,
) -> Result<(), String> {
    let multithread: ID3D11Multithread = context
        .cast()
        .map_err(|error| format!("{label} has no ID3D11Multithread state: {error}"))?;
    if unsafe { multithread.GetMultithreadProtected() }.as_bool() {
        // The replacement callback runs before the runtime acquires ID3D11Multithread's internal
        // lock, so concurrent legal calls could be ordered differently in our tracker and D3D11.
        return Err(format!(
            "{label} has runtime multithread protection enabled; DSV callback order cannot be proven"
        ));
    }
    Ok(())
}

fn install_multithread_protection_hook(context: &ID3D11DeviceContext) -> Result<(), String> {
    let multithread: ID3D11Multithread = context
        .cast()
        .map_err(|error| format!("game context has no ID3D11Multithread interface: {error}"))?;
    if let Some(existing) = GAME_MULTITHREAD_CONTEXT.get()
        && existing.as_raw() != multithread.as_raw()
    {
        return Err("game ID3D11Multithread interface identity changed".into());
    }
    if GAME_MULTITHREAD_CONTEXT.get().is_none() {
        GAME_MULTITHREAD_CONTEXT
            .set(multithread.clone())
            .map_err(|_| "game multithread context initialization raced".to_string())?;
    }
    if SET_MULTITHREAD_PROTECTED_HOOK.get().is_none() {
        verify_layout(
            multithread.as_raw(),
            d3d11_multithread::SET_MULTITHREAD_PROTECTED,
            "d3d11.dll",
        )?;
        let hook = unsafe {
            VtableHook::install_with_published_original(
                multithread.as_raw(),
                d3d11_multithread::SET_MULTITHREAD_PROTECTED,
                hooked_set_multithread_protected as *const () as usize,
                &ORIGINAL_SET_MULTITHREAD_PROTECTED,
            )
            .map_err(|error| format!("install SetMultithreadProtected hook failed: {error}"))?
        };
        SET_MULTITHREAD_PROTECTED_HOOK
            .set(hook)
            .map_err(|_| "SetMultithreadProtected hook initialization raced".to_string())?;
    }
    Ok(())
}

fn audit_multithread_protection_hook(callback_gap_possible: bool) -> Result<(), String> {
    let result = (|| {
        let context = GAME_MULTITHREAD_CONTEXT
            .get()
            .ok_or("game multithread context missing")?;
        let hook = SET_MULTITHREAD_PROTECTED_HOOK
            .get()
            .ok_or("SetMultithreadProtected hook missing")?;
        match unsafe {
            hook.state(
                context.as_raw(),
                d3d11_multithread::SET_MULTITHREAD_PROTECTED,
            )
        } {
            VtableSlotState::Installed => Ok(()),
            VtableSlotState::RestoredOriginal => {
                unsafe {
                    hook.rearm_if_restored(
                        context.as_raw(),
                        d3d11_multithread::SET_MULTITHREAD_PROTECTED,
                    )
                }?;
                if callback_gap_possible {
                    crate::dsv_discovery::record_integrity_failure(
                        crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
                    );
                }
                Ok(())
            }
            state => Err(format!(
                "SetMultithreadProtected hook integrity failed: {state:?}"
            )),
        }
    })();
    if result.is_err() {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
        );
    }
    result
}

fn install_deferred_context_hook(
    context: *mut core::ffi::c_void,
    index: usize,
    replacement: usize,
    original: &'static AtomicUsize,
    slot: &'static OnceLock<VtableHook>,
    label: &str,
) -> Result<(), String> {
    if slot.get().is_some() {
        return Ok(());
    }
    verify_layout(context, index, "d3d11.dll")?;
    let hook = unsafe {
        VtableHook::install_with_published_original(context, index, replacement, original)
            .map_err(|error| format!("install deferred {label} hook failed: {error}"))?
    };
    slot.set(hook)
        .map_err(|_| format!("deferred {label} hook initialization raced"))?;
    Ok(())
}

/// Patch the implementation vtable obtained from a real deferred context created by the game
/// device. D3D11 runtimes share that implementation table across deferred contexts, so this also
/// reaches contexts created before injection. A context using another table remains undecoded and
/// its command list is rejected at ExecuteCommandList.
fn install_deferred_dsv_discovery_hooks(device: &ID3D11Device) -> Result<(), String> {
    if DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire) {
        return Ok(());
    }
    if let Some(existing) = DSV_DISCOVERY_DEVICE.get() {
        if existing.as_raw() != device.as_raw() {
            return Err("D3D11 discovery device changed before deferred hooks became ready".into());
        }
    } else {
        DSV_DISCOVERY_DEVICE
            .set(device.clone())
            .map_err(|_| "D3D11 discovery device initialization raced".to_string())?;
    }
    let mut deferred = None;
    unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }
        .map_err(|error| format!("CreateDeferredContext for DSV ledger failed: {error}"))?;
    let deferred = deferred.ok_or("CreateDeferredContext returned no context")?;
    if unsafe { deferred.GetType() } != D3D11_DEVICE_CONTEXT_DEFERRED {
        return Err("CreateDeferredContext returned a non-deferred context".into());
    }
    let supports_context1 =
        verify_context_inherited_aliases(&deferred, "deferred discovery context")?;
    let raw = deferred.as_raw();
    let immediate = unsafe { device.GetImmediateContext() }.map_err(|error| {
        format!("GetImmediateContext for deferred vtable check failed: {error}")
    })?;
    if context_vtable_address(raw) == context_vtable_address(immediate.as_raw()) {
        return Err(
            "deferred and immediate contexts share one vtable; separate forwarding targets cannot be proven"
                .into(),
        );
    }

    install_deferred_context_hook(
        raw,
        d3d11_context::OM_SET_RENDER_TARGETS,
        hooked_deferred_om_set_render_targets as *const () as usize,
        &ORIGINAL_DEFERRED_OM_SET_RENDER_TARGETS,
        &DEFERRED_OM_SET_RENDER_TARGETS_HOOK,
        "OMSetRenderTargets",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
        hooked_deferred_om_set_render_targets_and_unordered_access_views as *const () as usize,
        &ORIGINAL_DEFERRED_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
        &DEFERRED_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK,
        "OMSetRenderTargetsAndUnorderedAccessViews",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::CLEAR_DEPTH_STENCIL_VIEW,
        hooked_deferred_clear_depth_stencil_view as *const () as usize,
        &ORIGINAL_DEFERRED_CLEAR_DEPTH_STENCIL_VIEW,
        &DEFERRED_CLEAR_DEPTH_STENCIL_VIEW_HOOK,
        "ClearDepthStencilView",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::CLEAR_STATE,
        hooked_deferred_clear_state as *const () as usize,
        &ORIGINAL_DEFERRED_CLEAR_STATE,
        &DEFERRED_CLEAR_STATE_HOOK,
        "ClearState",
    )?;
    if supports_context1 {
        install_deferred_context_hook(
            raw,
            d3d11_context::SWAP_DEVICE_CONTEXT_STATE,
            hooked_deferred_swap_device_context_state as *const () as usize,
            &ORIGINAL_DEFERRED_SWAP_DEVICE_CONTEXT_STATE,
            &DEFERRED_SWAP_DEVICE_CONTEXT_STATE_HOOK,
            "SwapDeviceContextState",
        )?;
        install_deferred_context_hook(
            raw,
            d3d11_context::CLEAR_VIEW,
            hooked_deferred_clear_view as *const () as usize,
            &ORIGINAL_DEFERRED_CLEAR_VIEW,
            &DEFERRED_CLEAR_VIEW_HOOK,
            "ClearView",
        )?;
    }
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW,
        hooked_deferred_draw as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW,
        &DEFERRED_DRAW_HOOK,
        "Draw",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW_INDEXED,
        hooked_deferred_draw_indexed as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW_INDEXED,
        &DEFERRED_DRAW_INDEXED_HOOK,
        "DrawIndexed",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW_INDEXED_INSTANCED,
        hooked_deferred_draw_indexed_instanced as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW_INDEXED_INSTANCED,
        &DEFERRED_DRAW_INDEXED_INSTANCED_HOOK,
        "DrawIndexedInstanced",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW_INSTANCED,
        hooked_deferred_draw_instanced as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW_INSTANCED,
        &DEFERRED_DRAW_INSTANCED_HOOK,
        "DrawInstanced",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW_AUTO,
        hooked_deferred_draw_auto as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW_AUTO,
        &DEFERRED_DRAW_AUTO_HOOK,
        "DrawAuto",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT,
        hooked_deferred_draw_indexed_instanced_indirect as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW_INDEXED_INSTANCED_INDIRECT,
        &DEFERRED_DRAW_INDEXED_INSTANCED_INDIRECT_HOOK,
        "DrawIndexedInstancedIndirect",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::DRAW_INSTANCED_INDIRECT,
        hooked_deferred_draw_instanced_indirect as *const () as usize,
        &ORIGINAL_DEFERRED_DRAW_INSTANCED_INDIRECT,
        &DEFERRED_DRAW_INSTANCED_INDIRECT_HOOK,
        "DrawInstancedIndirect",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::BEGIN,
        hooked_deferred_begin as *const () as usize,
        &ORIGINAL_DEFERRED_BEGIN,
        &DEFERRED_BEGIN_HOOK,
        "Begin",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::END,
        hooked_deferred_end as *const () as usize,
        &ORIGINAL_DEFERRED_END,
        &DEFERRED_END_HOOK,
        "End",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::EXECUTE_COMMAND_LIST,
        hooked_deferred_execute_command_list as *const () as usize,
        &ORIGINAL_DEFERRED_EXECUTE_COMMAND_LIST,
        &DEFERRED_EXECUTE_COMMAND_LIST_HOOK,
        "ExecuteCommandList",
    )?;
    install_deferred_context_hook(
        raw,
        d3d11_context::FINISH_COMMAND_LIST,
        hooked_deferred_finish_command_list as *const () as usize,
        &ORIGINAL_DEFERRED_FINISH_COMMAND_LIST,
        &DEFERRED_FINISH_COMMAND_LIST_HOOK,
        "FinishCommandList",
    )?;
    DEFERRED_DUMMY_CONTEXT
        .set(deferred)
        .map_err(|_| "deferred DSV dummy context initialization raced".to_string())?;
    DEFERRED_DSV_HOOKS_READY.store(true, Ordering::Release);
    log("D3D11 bounded deferred command-list DSV hooks installed");
    Ok(())
}

fn deferred_dsv_integrity_hooks() -> Result<Vec<(&'static str, usize, &'static VtableHook)>, String>
{
    let mut hooks = vec![
        (
            "deferred_om_set_render_targets",
            d3d11_context::OM_SET_RENDER_TARGETS,
            DEFERRED_OM_SET_RENDER_TARGETS_HOOK
                .get()
                .ok_or("deferred OMSetRenderTargets hook missing")?,
        ),
        (
            "deferred_om_set_render_targets_and_uavs",
            d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
            DEFERRED_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK
                .get()
                .ok_or("deferred OMSetRenderTargetsAndUnorderedAccessViews hook missing")?,
        ),
        (
            "deferred_clear_depth_stencil_view",
            d3d11_context::CLEAR_DEPTH_STENCIL_VIEW,
            DEFERRED_CLEAR_DEPTH_STENCIL_VIEW_HOOK
                .get()
                .ok_or("deferred ClearDepthStencilView hook missing")?,
        ),
        (
            "deferred_clear_state",
            d3d11_context::CLEAR_STATE,
            DEFERRED_CLEAR_STATE_HOOK
                .get()
                .ok_or("deferred ClearState hook missing")?,
        ),
        (
            "deferred_draw",
            d3d11_context::DRAW,
            DEFERRED_DRAW_HOOK
                .get()
                .ok_or("deferred Draw hook missing")?,
        ),
        (
            "deferred_draw_indexed",
            d3d11_context::DRAW_INDEXED,
            DEFERRED_DRAW_INDEXED_HOOK
                .get()
                .ok_or("deferred DrawIndexed hook missing")?,
        ),
        (
            "deferred_draw_indexed_instanced",
            d3d11_context::DRAW_INDEXED_INSTANCED,
            DEFERRED_DRAW_INDEXED_INSTANCED_HOOK
                .get()
                .ok_or("deferred DrawIndexedInstanced hook missing")?,
        ),
        (
            "deferred_draw_instanced",
            d3d11_context::DRAW_INSTANCED,
            DEFERRED_DRAW_INSTANCED_HOOK
                .get()
                .ok_or("deferred DrawInstanced hook missing")?,
        ),
        (
            "deferred_draw_auto",
            d3d11_context::DRAW_AUTO,
            DEFERRED_DRAW_AUTO_HOOK
                .get()
                .ok_or("deferred DrawAuto hook missing")?,
        ),
        (
            "deferred_draw_indexed_instanced_indirect",
            d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT,
            DEFERRED_DRAW_INDEXED_INSTANCED_INDIRECT_HOOK
                .get()
                .ok_or("deferred DrawIndexedInstancedIndirect hook missing")?,
        ),
        (
            "deferred_draw_instanced_indirect",
            d3d11_context::DRAW_INSTANCED_INDIRECT,
            DEFERRED_DRAW_INSTANCED_INDIRECT_HOOK
                .get()
                .ok_or("deferred DrawInstancedIndirect hook missing")?,
        ),
        (
            "deferred_begin",
            d3d11_context::BEGIN,
            DEFERRED_BEGIN_HOOK
                .get()
                .ok_or("deferred Begin hook missing")?,
        ),
        (
            "deferred_end",
            d3d11_context::END,
            DEFERRED_END_HOOK.get().ok_or("deferred End hook missing")?,
        ),
        (
            "deferred_execute_command_list",
            d3d11_context::EXECUTE_COMMAND_LIST,
            DEFERRED_EXECUTE_COMMAND_LIST_HOOK
                .get()
                .ok_or("deferred ExecuteCommandList hook missing")?,
        ),
        (
            "deferred_finish_command_list",
            d3d11_context::FINISH_COMMAND_LIST,
            DEFERRED_FINISH_COMMAND_LIST_HOOK
                .get()
                .ok_or("deferred FinishCommandList hook missing")?,
        ),
    ];
    if let Some(hook) = DEFERRED_SWAP_DEVICE_CONTEXT_STATE_HOOK.get() {
        hooks.push((
            "deferred_swap_device_context_state",
            d3d11_context::SWAP_DEVICE_CONTEXT_STATE,
            hook,
        ));
    }
    if let Some(hook) = DEFERRED_CLEAR_VIEW_HOOK.get() {
        hooks.push(("deferred_clear_view", d3d11_context::CLEAR_VIEW, hook));
    }
    Ok(hooks)
}

fn verify_deferred_context_hook_coverage(context: &ID3D11DeviceContext) -> Result<(), String> {
    let supports_context1 = verify_context_inherited_aliases(context, "observed deferred context")?;
    let context1_hooks_installed = DEFERRED_SWAP_DEVICE_CONTEXT_STATE_HOOK.get().is_some()
        && DEFERRED_CLEAR_VIEW_HOOK.get().is_some();
    if supports_context1 != context1_hooks_installed {
        return Err(format!(
            "observed deferred Context1 support={supports_context1} differs from dummy hook coverage={context1_hooks_installed}"
        ));
    }
    for (label, index, hook) in deferred_dsv_integrity_hooks()? {
        let state = unsafe { hook.state(context.as_raw(), index) };
        if state != VtableSlotState::Installed {
            return Err(format!(
                "{label} is not the exact installed slot on observed context: {state:?}"
            ));
        }
    }
    Ok(())
}

fn audit_deferred_dsv_hook_integrity(callback_gap_possible: bool) -> Result<(), String> {
    if !DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire) {
        return Err("deferred DSV hooks are not ready".into());
    }
    let context = DEFERRED_DUMMY_CONTEXT
        .get()
        .ok_or("deferred DSV dummy context missing")?;
    verify_context_inherited_aliases(context, "deferred discovery audit context").inspect_err(
        |_error| {
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
            );
        },
    )?;
    let mut failures = Vec::new();
    let mut restored = 0usize;
    for (label, index, hook) in deferred_dsv_integrity_hooks()? {
        match unsafe { hook.state(context.as_raw(), index) } {
            VtableSlotState::Installed => {}
            VtableSlotState::RestoredOriginal => {
                match unsafe { hook.rearm_if_restored(context.as_raw(), index) } {
                    Ok(true) => restored += 1,
                    Ok(false) => {}
                    Err(error) => failures.push(format!("{label}: {error}")),
                }
            }
            state => failures.push(format!("{label}: {state:?}")),
        }
    }
    if callback_gap_possible {
        for _ in 0..restored {
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
            );
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
        );
        Err(format!(
            "D3D11 deferred DSV hook integrity failed: {}",
            failures.join("; ")
        ))
    }
}

fn ensure_dsv_hooks_on_game_context(swapchain: *mut core::ffi::c_void) -> Result<(), String> {
    let _guard = DSV_HOOK_INSTALL_LOCK
        .lock()
        .map_err(|_| "B0 hook 安装锁已中毒")?;
    if game_context_hooks_installed() {
        return Ok(());
    }

    let sc =
        unsafe { IDXGISwapChain::from_raw_borrowed(&swapchain) }.ok_or("swapchain 指针无效")?;
    let device: ID3D11Device = unsafe { sc.GetDevice() }.map_err(|e| format!("GetDevice: {e}"))?;
    let ctx: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
        .map_err(|e| format!("拿不到 immediate context: {e}"))?;
    let supports_context1 = if crate::dsv_discovery::enabled() {
        verify_immediate_context_is_runtime_unprotected(&ctx, "game immediate discovery context")?;
        install_multithread_protection_hook(&ctx)?;
        verify_context_inherited_aliases(&ctx, "game immediate discovery context")?
    } else {
        ctx.cast::<ID3D11DeviceContext1>()
            .is_ok_and(|context1| context1.as_raw() == ctx.as_raw())
    };
    if crate::dsv_discovery::enabled() {
        install_deferred_dsv_discovery_hooks(&device)?;
    }
    let ctx_raw = ctx.as_raw();
    let previous_context = GAME_IMMEDIATE_CONTEXT.compare_exchange(
        0,
        ctx_raw as usize,
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    if let Err(existing) = previous_context
        && existing != ctx_raw as usize
    {
        return Err("游戏 immediate context 在同一 run 内发生变化，拒绝混合采样".into());
    }

    if OM_SET_RENDER_TARGETS_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::OM_SET_RENDER_TARGETS, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::OM_SET_RENDER_TARGETS,
                hooked_om_set_render_targets as *const () as usize,
            )
            .map_err(|e| format!("安装 OMSetRenderTargets hook 失败: {e}"))?
        };
        ORIGINAL_OM_SET_RENDER_TARGETS.store(hook.original(), Ordering::SeqCst);
        let _ = OM_SET_RENDER_TARGETS_HOOK.set(hook);
    }

    if OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK
        .get()
        .is_none()
    {
        verify_layout(
            ctx_raw,
            d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
            "d3d11.dll",
        )?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
                hooked_om_set_render_targets_and_unordered_access_views as *const () as usize,
            )
            .map_err(|e| format!("安装 OMSetRenderTargetsAndUnorderedAccessViews hook 失败: {e}"))?
        };
        ORIGINAL_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS
            .store(hook.original(), Ordering::SeqCst);
        let _ = OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS_HOOK.set(hook);
    }

    // P4 的三个 hook 与 DSV hook 同批安装：它们都要装在**游戏实际使用的 context**
    // 的 vtable 上（#425 实测：dummy device 的 context vtable 代表不了游戏的）。
    install_depth_observation_hooks(ctx_raw)?;

    if CLEAR_DEPTH_STENCIL_VIEW_HOOK.get().is_none() {
        verify_layout(
            ctx_raw,
            d3d11_context::CLEAR_DEPTH_STENCIL_VIEW,
            "d3d11.dll",
        )?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::CLEAR_DEPTH_STENCIL_VIEW,
                hooked_clear_depth_stencil_view as *const () as usize,
            )
            .map_err(|e| format!("安装 ClearDepthStencilView hook 失败: {e}"))?
        };
        ORIGINAL_CLEAR_DEPTH_STENCIL_VIEW.store(hook.original(), Ordering::SeqCst);
        let _ = CLEAR_DEPTH_STENCIL_VIEW_HOOK.set(hook);
    }

    if CLEAR_STATE_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::CLEAR_STATE, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install_with_published_original(
                ctx_raw,
                d3d11_context::CLEAR_STATE,
                hooked_clear_state as *const () as usize,
                &ORIGINAL_CLEAR_STATE,
            )
            .map_err(|e| format!("安装 ClearState hook 失败: {e}"))?
        };
        let _ = CLEAR_STATE_HOOK.set(hook);
    }

    if supports_context1 && SWAP_DEVICE_CONTEXT_STATE_HOOK.get().is_none() {
        verify_layout(
            ctx_raw,
            d3d11_context::SWAP_DEVICE_CONTEXT_STATE,
            "d3d11.dll",
        )?;
        let hook = unsafe {
            VtableHook::install_with_published_original(
                ctx_raw,
                d3d11_context::SWAP_DEVICE_CONTEXT_STATE,
                hooked_swap_device_context_state as *const () as usize,
                &ORIGINAL_SWAP_DEVICE_CONTEXT_STATE,
            )
            .map_err(|e| format!("安装 SwapDeviceContextState hook 失败: {e}"))?
        };
        let _ = SWAP_DEVICE_CONTEXT_STATE_HOOK.set(hook);
    }

    if supports_context1 && CLEAR_VIEW_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::CLEAR_VIEW, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install_with_published_original(
                ctx_raw,
                d3d11_context::CLEAR_VIEW,
                hooked_clear_view as *const () as usize,
                &ORIGINAL_CLEAR_VIEW,
            )
            .map_err(|e| format!("安装 ClearView hook 失败: {e}"))?
        };
        let _ = CLEAR_VIEW_HOOK.set(hook);
    }

    if EXECUTE_COMMAND_LIST_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::EXECUTE_COMMAND_LIST, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install_with_published_original(
                ctx_raw,
                d3d11_context::EXECUTE_COMMAND_LIST,
                hooked_execute_command_list as *const () as usize,
                &ORIGINAL_EXECUTE_COMMAND_LIST,
            )
            .map_err(|e| format!("安装 ExecuteCommandList hook 失败: {e}"))?
        };
        let _ = EXECUTE_COMMAND_LIST_HOOK.set(hook);
    }

    if game_context_shadow_required() && GAME_CONTEXT_VTABLE_SHADOW.get().is_none() {
        let (entry_count, interface) = context_shadow_vtable_shape(&ctx);
        let shadow = unsafe {
            VtableShadow::capture_and_activate(ctx_raw, entry_count)
                .map_err(|error| format!("创建 game context shadow vtable 失败: {error}"))?
        };
        let backing = shadow.backing_vtable_address();
        let active = shadow.shadow_vtable_address();
        GAME_CONTEXT_VTABLE_SHADOW
            .set(shadow)
            .map_err(|_| "game context shadow vtable 初始化竞争".to_string())?;
        log(&format!(
            "B0 game context shadow vtable 已启用 context={:#x} backing={backing:#x} shadow={active:#x} interface={interface} entries={entry_count}",
            ctx_raw as usize,
        ));
    }

    log("B0 game context hooks 已安装，开始枚举 DSV");
    CAMERA_HOOK_AUDIT_DUE.store(true, Ordering::Release);
    PRODUCTION_VS_QUERY_LOGGED.store(false, Ordering::Release);
    Ok(())
}

unsafe extern "system" fn hooked_om_set_render_targets(
    context: *mut core::ffi::c_void,
    num_views: u32,
    render_target_views: *const *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
) {
    let original = ORIGINAL_OM_SET_RENDER_TARGETS.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: OmSetRenderTargetsFn = unsafe { core::mem::transmute(original) };
    track_om_depth_binding(
        context,
        OmBindMethod::SetRenderTargets,
        OmDepthBindingAction::Set(depth_stencil_view),
        || unsafe { original(context, num_views, render_target_views, depth_stencil_view) },
    );
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hooked_om_set_render_targets_and_unordered_access_views(
    context: *mut core::ffi::c_void,
    num_views: u32,
    render_target_views: *const *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
    uav_start_slot: u32,
    num_uavs: u32,
    unordered_access_views: *const *mut core::ffi::c_void,
    uav_initial_counts: *const u32,
) {
    let original = ORIGINAL_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: OmSetRenderTargetsAndUnorderedAccessViewsFn =
        unsafe { core::mem::transmute(original) };
    track_om_depth_binding(
        context,
        OmBindMethod::SetRenderTargetsAndUnorderedAccessViews,
        om_uav_depth_binding_action(num_views, depth_stencil_view),
        || unsafe {
            original(
                context,
                num_views,
                render_target_views,
                depth_stencil_view,
                uav_start_slot,
                num_uavs,
                unordered_access_views,
                uav_initial_counts,
            )
        },
    );
}

fn om_uav_depth_binding_action(
    num_views: u32,
    depth_stencil_view: *mut core::ffi::c_void,
) -> OmDepthBindingAction {
    if num_views == D3D11_KEEP_RENDER_TARGETS_AND_DEPTH_STENCIL {
        OmDepthBindingAction::Keep
    } else {
        OmDepthBindingAction::Set(depth_stencil_view)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DsvDrawReacquireRoute {
    /// Normal pass recovery is descriptor-authoritative. If the old exact pair was explicitly
    /// unbound, the FSM may admit either that same pair or one serial replacement.
    RuntimeDescriptor,
    /// A phase reset observed no current DSV but retained the last exact pair. Only that pair may
    /// cross the phase boundary; another exact descriptor match is a permanent ambiguity.
    PhaseResetExactPair(DsvPairIdentity),
}

impl DsvDrawReacquireRoute {
    fn expected_pair(self) -> Option<DsvPairIdentity> {
        match self {
            Self::RuntimeDescriptor => None,
            Self::PhaseResetExactPair(pair) => Some(pair),
        }
    }
}

fn dsv_draw_reacquire_route(
    phase_reset_resource: usize,
    phase_reset_view: usize,
) -> Result<DsvDrawReacquireRoute, String> {
    match (phase_reset_resource, phase_reset_view) {
        (0, 0) => Ok(DsvDrawReacquireRoute::RuntimeDescriptor),
        (resource, view) if resource != 0 && view != 0 => Ok(
            DsvDrawReacquireRoute::PhaseResetExactPair(DsvPairIdentity {
                resource_tag: resource as u64,
                view_tag: view as u64,
            }),
        ),
        _ => {
            Err("phase-reset DSV reacquire latch contains only half of a view/resource pair".into())
        }
    }
}

fn track_om_depth_binding(
    context: *mut core::ffi::c_void,
    method: OmBindMethod,
    action: OmDepthBindingAction,
    forward_original: impl FnOnce(),
) {
    crate::perf::count(crate::perf::Counter::Om);
    if !depth_capture_enabled() {
        forward_original();
        return;
    }
    if !game_context_matches(context) {
        if dsv_discovery_observation_active() {
            crate::dsv_discovery::record_hook_drop();
        }
        if crate::camera_probe::discovery_enabled() {
            record_foreign_camera_om_bind_entry(method, context);
        }
        forward_original();
        return;
    }
    if crate::camera_probe::discovery_enabled() {
        record_camera_om_bind_entry(method);
    }
    let OmDepthBindingAction::Set(depth_stencil_view) = action else {
        // KEEP 表示 DSV 不变；不能把被忽略的空指针当成解绑，也不能重复做末帧快照。
        forward_original();
        return;
    };
    let discovery_guard = enter_owned_discovery_callback();
    if CAMERA_DISCOVERY_V3_EXACT_ACTIVE.load(Ordering::Acquire) && discovery_guard.is_none() {
        // stop_discovery has closed the old epoch (or Start ACK has not opened the new one).
        // Do not let an unowned callback mutate pending-unbind/pair state across that boundary.
        DSV_BINDING_DIRTY.store(true, Ordering::Release);
        forward_original();
        return;
    }
    if !dsv_observation_frame_active() {
        // 非证据帧绝不 QueryInterface/GetDesc/拿 DSV tracker 锁。只留一个原子脏标记，
        // 下一证据帧在首个 Draw 从真实 OM state 精确重获。
        DSV_BINDING_DIRTY.store(true, Ordering::Release);
        forward_original();
        return;
    }
    if dsv_discovery_observation_active() {
        if let Some(entry) = crate::dsv_discovery::try_enter_observation()
            && let Err(error) = record_dsv_binding_admitted(depth_stencil_view)
        {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
            );
            if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                log(&format!("D3D11 DSV discovery OM 账本失败: {error}"));
            }
        }
        forward_original();
        return;
    }
    if DSV_BINDING_DIRTY.load(Ordering::Acquire) {
        let stale_current = match CURRENT_SELECTED_DEPTH.lock() {
            Ok(mut current) => {
                // 跳过的 production 帧可能已经换过多次 DSV；旧 COM 引用只能用于保活，
                // 不能作为本证据帧的 outgoing pass 快照。
                let stale = current.take();
                CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                stale
            }
            Err(poisoned) => {
                drop(poisoned.into_inner());
                CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                DSV_BINDING_DIRTY.store(true, Ordering::Release);
                latch_exact_after_lock_failure(
                    SelectedDepthLockFailure::Poisoned,
                    discovery_guard.as_ref(),
                );
                forward_original();
                return;
            }
        };
        if stale_current.is_some()
            && update_retained_unbound_selected_depth(stale_current, None, discovery_guard.as_ref())
                .is_err()
        {
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            forward_original();
            return;
        }
    }
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let outgoing_pair = match CURRENT_SELECTED_DEPTH.lock() {
        Ok(current) => current.as_ref().map(SelectedDepth::pair),
        Err(poisoned) => {
            drop(poisoned.into_inner());
            CURRENT_SELECTED_RAW.store(0, Ordering::Release);
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            latch_exact_after_lock_failure(
                SelectedDepthLockFailure::Poisoned,
                discovery_guard.as_ref(),
            );
            forward_original();
            return;
        }
    };
    let reacquire_route = dsv_draw_reacquire_route(
        DISCOVERY_DSV_REACQUIRE_RESOURCE.load(Ordering::Acquire),
        DISCOVERY_DSV_REACQUIRE_VIEW.load(Ordering::Acquire),
    );
    let incoming = if depth_stencil_view.is_null() {
        if let Some(pair) = outgoing_pair {
            match DEPTH_SELECTION.lock() {
                Ok(mut selection) => {
                    selection.note_explicit_unbind(pair);
                    Ok(None)
                }
                Err(poisoned) => {
                    drop(poisoned.into_inner());
                    if latch_exact_after_lock_failure(
                        SelectedDepthLockFailure::Poisoned,
                        discovery_guard.as_ref(),
                    ) {
                        CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                        DSV_BINDING_DIRTY.store(true, Ordering::Release);
                    }
                    Err("depth selection lock poisoned while recording explicit DSV unbind".into())
                }
            }
        } else {
            Ok(None)
        }
    } else {
        match reacquire_route.as_ref() {
            Ok(route) => selected_depth_after_observed_unbind(
                depth_stencil_view,
                route.expected_pair(),
                discovery_guard.as_ref(),
            ),
            Err(error) => {
                latch_exact_depth_selection_ambiguity(discovery_guard.as_ref());
                Err(error.clone())
            }
        }
    };

    if let Err(error) = record_dsv_binding(depth_stencil_view) {
        let exact_binding_failure = latch_exact_depth_selection_ambiguity(discovery_guard.as_ref());
        if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
            log(&format!("B0 DSV 绑定账本失败: {error}"));
        }
        if exact_binding_failure {
            DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
            DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
            CURRENT_SELECTED_RAW.store(0, Ordering::Release);
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            forward_original();
            return;
        }
    }

    if !depth_stencil_view.is_null() {
        if let Err(error) = &incoming
            && !CAPTURE_ERROR_LOGGED.swap(true, Ordering::Relaxed)
        {
            log(&format!("P1c 识别主场景 DSV 失败: {error}"));
        }
        if incoming.is_err() {
            DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
            DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
            match CURRENT_SELECTED_DEPTH.lock() {
                Ok(mut current) => {
                    *current = None;
                    CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                }
                Err(poisoned) => {
                    drop(poisoned.into_inner());
                    CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                    latch_exact_after_lock_failure(
                        SelectedDepthLockFailure::Poisoned,
                        discovery_guard.as_ref(),
                    );
                }
            }
        }
    }

    let mut exact_snapshot_failure = false;
    if let Ok(incoming) = &incoming
        && let Err(error) = snapshot_outgoing_selected(context, incoming.as_ref())
    {
        exact_snapshot_failure = latch_exact_depth_selection_ambiguity(discovery_guard.as_ref());
        if !CAPTURE_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
            log(&format!("P1c 主场景 DSV 解绑快照失败: {error}"));
        }
    }

    forward_original();

    if exact_snapshot_failure {
        DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
        DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
        match CURRENT_SELECTED_DEPTH.lock() {
            Ok(mut current) => {
                *current = None;
                CURRENT_SELECTED_RAW.store(0, Ordering::Release);
            }
            Err(poisoned) => {
                drop(poisoned.into_inner());
                CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                latch_exact_after_lock_failure(
                    SelectedDepthLockFailure::Poisoned,
                    discovery_guard.as_ref(),
                );
            }
        }
        DSV_BINDING_DIRTY.store(true, Ordering::Release);
        return;
    }

    if let Ok(incoming) = incoming {
        let mut current = match CURRENT_SELECTED_DEPTH.lock() {
            Ok(current) => current,
            Err(poisoned) => {
                drop(poisoned.into_inner());
                CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                DSV_BINDING_DIRTY.store(true, Ordering::Release);
                latch_exact_after_lock_failure(
                    SelectedDepthLockFailure::Poisoned,
                    discovery_guard.as_ref(),
                );
                return;
            }
        };
        let outgoing = current.as_ref().cloned();
        let incoming_pair = incoming.as_ref().map(SelectedDepth::pair);
        let incoming_identity = incoming.as_ref().map(|next| next.identity);
        let incoming_resource = incoming_pair.map(|pair| pair.resource_tag as usize);
        let begins_main_pass = incoming.as_ref().is_some_and(|next| {
            current
                .as_ref()
                .is_none_or(|previous| previous.identity != next.identity)
        });
        *current = incoming;
        CURRENT_SELECTED_RAW.store(incoming_resource.unwrap_or(0), Ordering::Release);
        DSV_BINDING_DIRTY.store(false, Ordering::Release);
        DSV_REACQUIRE_LAST_PRESENT.store(u64::MAX, Ordering::Release);
        drop(current);
        if update_retained_unbound_selected_depth(outgoing, incoming_pair, discovery_guard.as_ref())
            .is_err()
        {
            match CURRENT_SELECTED_DEPTH.lock() {
                Ok(mut current) => *current = None,
                Err(poisoned) => {
                    drop(poisoned.into_inner());
                    latch_exact_after_lock_failure(
                        SelectedDepthLockFailure::Poisoned,
                        discovery_guard.as_ref(),
                    );
                }
            }
            CURRENT_SELECTED_RAW.store(0, Ordering::Release);
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            return;
        }
        if begins_main_pass {
            SELECTED_PASS_DRAW_CALLS.store(0, Ordering::Release);
            crate::camera_probe::on_main_pass_bound(
                callback_generation,
                PRESENT_SEQ.load(Ordering::Relaxed),
            );
        }
        if let (Some(resource), Ok(DsvDrawReacquireRoute::PhaseResetExactPair(expected))) =
            (incoming_resource, reacquire_route)
            && incoming_identity.is_some_and(|identity| identity.matches_pair(expected))
        {
            DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
            DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
            DISCOVERY_DSV_REACQUIRE_LAST_PRESENT.store(u64::MAX, Ordering::Release);
            if !DISCOVERY_DSV_REACQUIRE_SUCCESS_LOGGED.swap(true, Ordering::AcqRel) {
                log(&format!(
                    "camera discovery 已通过 {} 重获受审主 DSV: resource={resource:#x}",
                    method.label()
                ));
            }
        }
    }
}

unsafe extern "system" fn hooked_clear_depth_stencil_view(
    context: *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
    clear_flags: u32,
    depth: f32,
    stencil: u8,
) {
    let original = ORIGINAL_CLEAR_DEPTH_STENCIL_VIEW.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        u32,
        f32,
        u8,
    ) = unsafe { core::mem::transmute(original) };
    if !game_context_matches(context) {
        if dsv_discovery_observation_active() {
            crate::dsv_discovery::record_hook_drop();
        }
        unsafe { original(context, depth_stencil_view, clear_flags, depth, stencil) };
        return;
    }
    if !dsv_observation_frame_active() {
        unsafe { original(context, depth_stencil_view, clear_flags, depth, stencil) };
        return;
    }

    if !depth_stencil_view.is_null() {
        let discovery_entry = if dsv_discovery_observation_active() {
            crate::dsv_discovery::try_enter_observation()
        } else {
            None
        };
        let result = if discovery_entry.is_some() {
            record_dsv_clear_admitted(depth_stencil_view)
        } else if dsv_discovery_observation_active() {
            Ok(())
        } else {
            record_dsv_clear(depth_stencil_view)
        };
        if let Err(error) = result {
            if let Some(entry) = &discovery_entry {
                entry.record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                );
            }
            if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                log(&format!("B0 DSV clear 账本失败: {error}"));
            }
        }
    }
    unsafe { original(context, depth_stencil_view, clear_flags, depth, stencil) };
}

unsafe extern "system" fn hooked_clear_state(context: *mut core::ffi::c_void) {
    let target = ORIGINAL_CLEAR_STATE.load(Ordering::SeqCst);
    if target == 0 {
        return;
    }
    if dsv_discovery_observation_active() {
        if game_context_matches(context) {
            if let Some(entry) = crate::dsv_discovery::try_enter_observation()
                && let Err(error) = record_dsv_binding_admitted(std::ptr::null_mut())
            {
                entry.record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                );
                if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                    log(&format!(
                        "D3D11 immediate ClearState DSV update failed: {error}"
                    ));
                }
            }
        } else {
            if let Some(entry) = crate::dsv_discovery::try_enter_observation() {
                entry.record_hook_drop();
            }
        }
    }
    let original: unsafe extern "system" fn(*mut core::ffi::c_void) =
        unsafe { core::mem::transmute(target) };
    unsafe { original(context) };
}

unsafe extern "system" fn hooked_set_multithread_protected(
    multithread: *mut core::ffi::c_void,
    protected: BOOL,
) -> BOOL {
    let target = ORIGINAL_SET_MULTITHREAD_PROTECTED.load(Ordering::SeqCst);
    if target == 0 {
        return BOOL(0);
    }
    type SetMultithreadProtectedFn =
        unsafe extern "system" fn(*mut core::ffi::c_void, BOOL) -> BOOL;
    let original: SetMultithreadProtectedFn = unsafe { core::mem::transmute(target) };
    let entry = if crate::dsv_discovery::enabled()
        && GAME_MULTITHREAD_CONTEXT
            .get()
            .is_some_and(|expected| expected.as_raw() == multithread)
    {
        crate::dsv_discovery::try_enter_observation()
    } else {
        None
    };
    let previous = unsafe { original(multithread, protected) };
    if multithread_protection_changed(previous.as_bool(), protected.as_bool())
        && let Some(entry) = entry
    {
        entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
        );
        if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
            log(
                "ID3D11Multithread protection changed during generic DSV discovery; callback ordering is no longer provable",
            );
        }
    }
    previous
}

const fn multithread_protection_changed(previous: bool, requested: bool) -> bool {
    previous != requested
}

unsafe extern "system" fn hooked_swap_device_context_state(
    context: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
    previous_state: *mut *mut core::ffi::c_void,
) {
    let target = ORIGINAL_SWAP_DEVICE_CONTEXT_STATE.load(Ordering::SeqCst);
    if target == 0 {
        return;
    }
    type SwapDeviceContextStateFn = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *mut *mut core::ffi::c_void,
    );
    let original: SwapDeviceContextStateFn = unsafe { core::mem::transmute(target) };
    let discovery_entry = if dsv_discovery_observation_active() && !state.is_null() {
        crate::dsv_discovery::try_enter_observation()
    } else {
        None
    };
    unsafe { original(context, state, previous_state) };
    if let Some(entry) = discovery_entry {
        if game_context_matches(context) {
            // A non-null state can switch the context into D3D10/10.1 emulation, after which
            // binding changes can bypass the D3D11 hooks entirely. There is no complete generic
            // reconstruction path, so the run is sticky-invalid even if OMGet looks plausible.
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
            );
            if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                log(
                    "D3D11 immediate SwapDeviceContextState(non-null) is unsupported by generic DSV discovery",
                );
            }
        } else {
            entry.record_hook_drop();
        }
    }
}

unsafe extern "system" fn hooked_clear_view(
    context: *mut core::ffi::c_void,
    view: *mut core::ffi::c_void,
    color: *const f32,
    rects: *const RECT,
    num_rects: u32,
) {
    let target = ORIGINAL_CLEAR_VIEW.load(Ordering::SeqCst);
    if target == 0 {
        return;
    }
    let discovery_entry = if dsv_discovery_observation_active() {
        crate::dsv_discovery::try_enter_observation()
    } else {
        None
    };
    if let Some(entry) = &discovery_entry {
        if game_context_matches(context) {
            if depth_stencil_view_from_generic_view(view).is_some() {
                // ClearView has DSV-specific depth-only and rectangle semantics. Treating it as
                // ClearDepthStencilView would manufacture clear evidence; keep the run invalid
                // until those semantics are modeled explicitly.
                entry.record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                );
                if !DSV_AUDIT_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                    log("D3D11 immediate ClearView on a DSV is unsupported by generic discovery");
                }
            }
        } else {
            entry.record_hook_drop();
        }
    }
    type ClearViewFn = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        *const f32,
        *const RECT,
        u32,
    );
    let original: ClearViewFn = unsafe { core::mem::transmute(target) };
    unsafe { original(context, view, color, rects, num_rects) };
}

unsafe extern "system" fn hooked_deferred_om_set_render_targets(
    context: *mut core::ffi::c_void,
    num_views: u32,
    render_target_views: *const *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
) {
    if deferred_dsv_ledger_capture_active() {
        record_deferred_dsv_bind(context, depth_stencil_view);
    }
    let target = ORIGINAL_DEFERRED_OM_SET_RENDER_TARGETS.load(Ordering::SeqCst);
    if target != 0 {
        let original: OmSetRenderTargetsFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, num_views, render_target_views, depth_stencil_view) };
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hooked_deferred_om_set_render_targets_and_unordered_access_views(
    context: *mut core::ffi::c_void,
    num_views: u32,
    render_target_views: *const *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
    uav_start_slot: u32,
    num_uavs: u32,
    unordered_access_views: *const *mut core::ffi::c_void,
    uav_initial_counts: *const u32,
) {
    if deferred_dsv_ledger_capture_active()
        && num_views != D3D11_KEEP_RENDER_TARGETS_AND_DEPTH_STENCIL
    {
        record_deferred_dsv_bind(context, depth_stencil_view);
    }
    let target =
        ORIGINAL_DEFERRED_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS.load(Ordering::SeqCst);
    if target != 0 {
        let original: OmSetRenderTargetsAndUnorderedAccessViewsFn =
            unsafe { core::mem::transmute(target) };
        unsafe {
            original(
                context,
                num_views,
                render_target_views,
                depth_stencil_view,
                uav_start_slot,
                num_uavs,
                unordered_access_views,
                uav_initial_counts,
            )
        };
    }
}

unsafe extern "system" fn hooked_deferred_clear_depth_stencil_view(
    context: *mut core::ffi::c_void,
    depth_stencil_view: *mut core::ffi::c_void,
    clear_flags: u32,
    depth: f32,
    stencil: u8,
) {
    if deferred_dsv_ledger_capture_active() && !depth_stencil_view.is_null() {
        match inspect_deferred_dsv(depth_stencil_view) {
            Ok(snapshot) => record_deferred_dsv_event(context, DeferredDsvEvent::Clear(snapshot)),
            Err(error) => {
                crate::dsv_discovery::record_integrity_failure(
                    crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                );
                if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                    log(&format!(
                        "D3D11 deferred clear inspection failed closed: {error}"
                    ));
                }
            }
        }
    }
    let target = ORIGINAL_DEFERRED_CLEAR_DEPTH_STENCIL_VIEW.load(Ordering::SeqCst);
    if target != 0 {
        let original: unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *mut core::ffi::c_void,
            u32,
            f32,
            u8,
        ) = unsafe { core::mem::transmute(target) };
        unsafe { original(context, depth_stencil_view, clear_flags, depth, stencil) };
    }
}

unsafe extern "system" fn hooked_deferred_clear_state(context: *mut core::ffi::c_void) {
    if deferred_dsv_ledger_capture_active() {
        record_deferred_dsv_event(context, DeferredDsvEvent::Bind(None));
    }
    let target = ORIGINAL_DEFERRED_CLEAR_STATE.load(Ordering::SeqCst);
    if target != 0 {
        let original: unsafe extern "system" fn(*mut core::ffi::c_void) =
            unsafe { core::mem::transmute(target) };
        unsafe { original(context) };
    }
}

unsafe extern "system" fn hooked_deferred_swap_device_context_state(
    context: *mut core::ffi::c_void,
    state: *mut core::ffi::c_void,
    previous_state: *mut *mut core::ffi::c_void,
) {
    // This method can replace every pipeline binding and has no result that proves the deferred
    // runtime accepted it. Keep the containing generation opaque.
    taint_deferred_dsv_generation(context, false);
    let target = ORIGINAL_DEFERRED_SWAP_DEVICE_CONTEXT_STATE.load(Ordering::SeqCst);
    if target != 0 {
        type SwapDeviceContextStateFn = unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *mut core::ffi::c_void,
            *mut *mut core::ffi::c_void,
        );
        let original: SwapDeviceContextStateFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, state, previous_state) };
    }
}

unsafe extern "system" fn hooked_deferred_clear_view(
    context: *mut core::ffi::c_void,
    view: *mut core::ffi::c_void,
    color: *const f32,
    rects: *const RECT,
    num_rects: u32,
) {
    if deferred_dsv_ledger_capture_active() && depth_stencil_view_from_generic_view(view).is_some()
    {
        taint_deferred_dsv_generation(context, false);
    }
    let target = ORIGINAL_DEFERRED_CLEAR_VIEW.load(Ordering::SeqCst);
    if target != 0 {
        type ClearViewFn = unsafe extern "system" fn(
            *mut core::ffi::c_void,
            *mut core::ffi::c_void,
            *const f32,
            *const RECT,
            u32,
        );
        let original: ClearViewFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, view, color, rects, num_rects) };
    }
}

fn record_deferred_draw(context: *mut core::ffi::c_void) {
    if deferred_dsv_ledger_capture_active() {
        record_deferred_dsv_event(context, DeferredDsvEvent::Draw);
    }
}

unsafe extern "system" fn hooked_deferred_draw(
    context: *mut core::ffi::c_void,
    vertex_count: u32,
    start_vertex_location: u32,
) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, vertex_count, start_vertex_location) };
    }
}

unsafe extern "system" fn hooked_deferred_draw_indexed(
    context: *mut core::ffi::c_void,
    index_count: u32,
    start_index_location: u32,
    base_vertex_location: i32,
) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW_INDEXED.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawIndexedFn = unsafe { core::mem::transmute(target) };
        unsafe {
            original(
                context,
                index_count,
                start_index_location,
                base_vertex_location,
            )
        };
    }
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hooked_deferred_draw_indexed_instanced(
    context: *mut core::ffi::c_void,
    index_count_per_instance: u32,
    instance_count: u32,
    start_index_location: u32,
    base_vertex_location: i32,
    start_instance_location: u32,
) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW_INDEXED_INSTANCED.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawIndexedInstancedFn = unsafe { core::mem::transmute(target) };
        unsafe {
            original(
                context,
                index_count_per_instance,
                instance_count,
                start_index_location,
                base_vertex_location,
                start_instance_location,
            )
        };
    }
}

unsafe extern "system" fn hooked_deferred_draw_instanced(
    context: *mut core::ffi::c_void,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex_location: u32,
    start_instance_location: u32,
) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW_INSTANCED.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawInstancedFn = unsafe { core::mem::transmute(target) };
        unsafe {
            original(
                context,
                vertex_count_per_instance,
                instance_count,
                start_vertex_location,
                start_instance_location,
            )
        };
    }
}

unsafe extern "system" fn hooked_deferred_draw_auto(context: *mut core::ffi::c_void) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW_AUTO.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawAutoFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context) };
    }
}

unsafe extern "system" fn hooked_deferred_draw_indexed_instanced_indirect(
    context: *mut core::ffi::c_void,
    buffer_for_args: *mut core::ffi::c_void,
    aligned_byte_offset_for_args: u32,
) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW_INDEXED_INSTANCED_INDIRECT.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawIndirectFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, buffer_for_args, aligned_byte_offset_for_args) };
    }
}

unsafe extern "system" fn hooked_deferred_draw_instanced_indirect(
    context: *mut core::ffi::c_void,
    buffer_for_args: *mut core::ffi::c_void,
    aligned_byte_offset_for_args: u32,
) {
    record_deferred_draw(context);
    let target = ORIGINAL_DEFERRED_DRAW_INSTANCED_INDIRECT.load(Ordering::SeqCst);
    if target != 0 {
        let original: DrawIndirectFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, buffer_for_args, aligned_byte_offset_for_args) };
    }
}

unsafe extern "system" fn hooked_deferred_begin(
    context: *mut core::ffi::c_void,
    asynchronous: *mut core::ffi::c_void,
) {
    // Query validity is reported only through the optional D3D debug layer while ExecuteCommandList
    // itself returns void. Never pre-replay a list whose runtime acceptance cannot be proven.
    taint_deferred_dsv_generation(context, false);
    let target = ORIGINAL_DEFERRED_BEGIN.load(Ordering::SeqCst);
    if target != 0 {
        let original: unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) =
            unsafe { core::mem::transmute(target) };
        unsafe { original(context, asynchronous) };
    }
}

unsafe extern "system" fn hooked_deferred_end(
    context: *mut core::ffi::c_void,
    asynchronous: *mut core::ffi::c_void,
) {
    taint_deferred_dsv_generation(context, false);
    let target = ORIGINAL_DEFERRED_END.load(Ordering::SeqCst);
    if target != 0 {
        let original: unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) =
            unsafe { core::mem::transmute(target) };
        unsafe { original(context, asynchronous) };
    }
}

unsafe extern "system" fn hooked_deferred_execute_command_list(
    context: *mut core::ffi::c_void,
    command_list: *mut core::ffi::c_void,
    restore_context_state: BOOL,
) {
    if deferred_dsv_ledger_capture_active() {
        let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
            let target = ORIGINAL_DEFERRED_EXECUTE_COMMAND_LIST.load(Ordering::SeqCst);
            if target != 0 {
                let original: ExecuteCommandListFn = unsafe { core::mem::transmute(target) };
                unsafe { original(context, command_list, restore_context_state) };
            }
            return;
        };
        match deferred_context_belongs_to_discovery_device(context) {
            Ok(true) => {
                // The source list is immutable. Clone its Arc under a short lookup guard, expand
                // outside the ledger, then append under a second short guard. This avoids holding
                // the global recording ledger while cloning up to the bounded event limit.
                let source = match deferred_dsv_ledger().lock() {
                    Ok(ledger) => ledger.complete_command_list_events(command_list),
                    Err(_) => {
                        entry.record_integrity_failure(
                            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                        );
                        None
                    }
                };
                let prepared = source.as_deref().map(prepare_nested_dsv_events);
                match deferred_dsv_ledger().lock() {
                    Ok(mut ledger) if deferred_dsv_ledger_capture_active() => {
                        let result = if let Some(prepared) = prepared {
                            ledger
                                .merge_prepared_events(
                                    context,
                                    prepared,
                                    restore_context_state.as_bool(),
                                )
                                .map(|()| true)
                        } else {
                            ledger
                                .taint_context(context, !restore_context_state.as_bool())
                                .map(|()| false)
                        };
                        if let Err(error) = result {
                            record_deferred_ledger_failure(&entry, &error);
                            if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED
                                .swap(true, Ordering::AcqRel)
                            {
                                log(&format!(
                                    "D3D11 nested deferred command-list merge failed closed: {error}"
                                ));
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        entry.record_integrity_failure(
                            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                        );
                    }
                }
            }
            Ok(false) => {}
            Err(_) => entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
            ),
        }
    }
    let target = ORIGINAL_DEFERRED_EXECUTE_COMMAND_LIST.load(Ordering::SeqCst);
    if target != 0 {
        let original: ExecuteCommandListFn = unsafe { core::mem::transmute(target) };
        unsafe { original(context, command_list, restore_context_state) };
    }
}

unsafe extern "system" fn hooked_deferred_finish_command_list(
    context: *mut core::ffi::c_void,
    restore_deferred_context_state: BOOL,
    command_list: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let target = ORIGINAL_DEFERRED_FINISH_COMMAND_LIST.load(Ordering::SeqCst);
    if target == 0 {
        return HRESULT(-1);
    }
    type FinishCommandListFn = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        BOOL,
        *mut *mut core::ffi::c_void,
    ) -> HRESULT;
    let original: FinishCommandListFn = unsafe { core::mem::transmute(target) };
    let result = unsafe { original(context, restore_deferred_context_state, command_list) };
    if !deferred_dsv_ledger_capture_active() {
        return result;
    }
    let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
        return result;
    };
    match deferred_context_belongs_to_discovery_device(context) {
        Ok(true) => {}
        Ok(false) => return result,
        Err(_) => {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
            );
            return result;
        }
    }
    if result.is_err() || command_list.is_null() {
        entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
        );
        return result;
    }
    let raw = unsafe { *command_list };
    let Some(owner) = (unsafe { ID3D11CommandList::from_raw_borrowed(&raw) }).cloned() else {
        entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
        );
        return result;
    };
    let sealed = deferred_dsv_ledger()
        .lock()
        .map_err(|_| "deferred DSV ledger lock poisoned at FinishCommandList".to_string())
        .and_then(|mut ledger| {
            if !deferred_dsv_ledger_capture_active() {
                return Ok(true);
            }
            ledger.seal_command_list(context, owner, restore_deferred_context_state.as_bool())
        });
    if let Err(error) = sealed {
        record_deferred_ledger_failure(&entry, &error);
        if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
            log(&format!(
                "D3D11 deferred command-list seal failed closed: {error}"
            ));
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeferredCommandListReplay {
    Replayed,
    Opaque,
    TrackerFailure,
}

fn replay_deferred_command_list(
    command_list: *mut core::ffi::c_void,
    restore_context_state: bool,
) -> DeferredCommandListReplay {
    // Clone only the immutable Arc while holding the ledger. Replaying up to the bounded event
    // limit must not block unrelated deferred recording/Finish callbacks on the global ledger.
    let events = {
        let ledger = match deferred_dsv_ledger().lock() {
            Ok(ledger) => ledger,
            Err(_) => return DeferredCommandListReplay::TrackerFailure,
        };
        let Some(recording) = ledger.command_lists.get(&(command_list as usize)) else {
            return DeferredCommandListReplay::Opaque;
        };
        if !recording.complete {
            return DeferredCommandListReplay::Opaque;
        }
        recording.events.clone()
    };
    let mut tracker = match dsv_frame_tracker().lock() {
        Ok(tracker) => tracker,
        Err(_) => return DeferredCommandListReplay::TrackerFailure,
    };
    tracker.replay_deferred(&events, restore_context_state);
    DeferredCommandListReplay::Replayed
}

unsafe extern "system" fn hooked_execute_command_list(
    context: *mut core::ffi::c_void,
    command_list: *mut core::ffi::c_void,
    restore_context_state: BOOL,
) {
    let original = ORIGINAL_EXECUTE_COMMAND_LIST.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: ExecuteCommandListFn = unsafe { core::mem::transmute(original) };

    match dsv_command_list_route(
        dsv_discovery_observation_active(),
        GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire),
        context as usize,
    ) {
        DsvCommandListRoute::Ignore => {}
        DsvCommandListRoute::ReplayOnGameContext => {
            if let Some(entry) = crate::dsv_discovery::try_enter_observation() {
                match replay_deferred_command_list(command_list, restore_context_state.as_bool()) {
                    DeferredCommandListReplay::Replayed => {}
                    DeferredCommandListReplay::Opaque => {
                        entry.record_integrity_failure(
                            crate::dsv_discovery::HookIntegrityFailureReason::UnsupportedExecuteCommandList,
                        );
                        if !DEFERRED_COMMAND_LIST_DISCOVERY_ERROR_LOGGED
                            .swap(true, Ordering::AcqRel)
                        {
                            log(
                                "D3D11 DSV discovery executed an unknown, pre-boundary, incomplete, or overflowed command list; report remains fail-closed",
                            );
                        }
                    }
                    DeferredCommandListReplay::TrackerFailure => {
                        entry.record_integrity_failure(
                            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
                        );
                    }
                }
            }
        }
        DsvCommandListRoute::ForeignContext => {
            if let Some(entry) = crate::dsv_discovery::try_enter_observation() {
                entry.record_hook_drop();
            }
        }
    }

    unsafe { original(context, command_list, restore_context_state) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DsvCommandListRoute {
    Ignore,
    ForeignContext,
    ReplayOnGameContext,
}

const fn dsv_command_list_route(
    observation_active: bool,
    expected_context: usize,
    actual_context: usize,
) -> DsvCommandListRoute {
    if !observation_active {
        DsvCommandListRoute::Ignore
    } else if expected_context != actual_context {
        DsvCommandListRoute::ForeignContext
    } else {
        DsvCommandListRoute::ReplayOnGameContext
    }
}

fn inspect_dsv_resource(
    depth_stencil_view: *mut core::ffi::c_void,
) -> Result<ObservedDsvResource, String> {
    let dsv = unsafe { ID3D11DepthStencilView::from_raw_borrowed(&depth_stencil_view) }
        .ok_or("DSV 指针无效")?;
    let resource = unsafe { dsv.GetResource() }.map_err(|e| format!("DSV GetResource: {e}"))?;
    let texture: ID3D11Texture2D = resource
        .cast()
        .map_err(|e| format!("DSV resource 不是 Texture2D: {e}"))?;
    // 与 `selected_depth` 使用同一种 COM interface pointer 做进程内 key。
    // `ID3D11Resource*` 与 QueryInterface 后的 `ID3D11Texture2D*` 数值不保证相同。
    let resource_id = texture.as_raw() as usize;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let mut view_desc = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
    unsafe { dsv.GetDesc(&mut view_desc) };
    let (mip_slice, first_array_slice, view_array_size) = match view_desc.ViewDimension {
        D3D11_DSV_DIMENSION_TEXTURE2D => (unsafe { view_desc.Anonymous.Texture2D.MipSlice }, 0, 1),
        D3D11_DSV_DIMENSION_TEXTURE2DARRAY => {
            let array = unsafe { view_desc.Anonymous.Texture2DArray };
            (array.MipSlice, array.FirstArraySlice, array.ArraySize)
        }
        D3D11_DSV_DIMENSION_TEXTURE2DMS => (0, 0, 1),
        D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY => {
            let array = unsafe { view_desc.Anonymous.Texture2DMSArray };
            (0, array.FirstArraySlice, array.ArraySize)
        }
        other => {
            return Err(format!(
                "DSV Texture2D 使用了不支持的 view dimension: {}",
                other.0
            ));
        }
    };

    let description = format!(
        "resource=0x{resource_id:X} {}x{} view_format={:?} resource_format={:?} samples={} quality={} bind=0x{:X}",
        desc.Width,
        desc.Height,
        view_desc.Format,
        desc.Format,
        desc.SampleDesc.Count,
        desc.SampleDesc.Quality,
        desc.BindFlags
    );
    let is_new = DSV_AUDIT_SEEN
        .get_or_init(|| Mutex::new(BTreeSet::new()))
        .lock()
        .map_err(|_| "DSV 去重锁已中毒")?
        .insert(description.clone());
    if is_new {
        log(&format!("B0 DSV {description}"));
    }
    Ok(ObservedDsvResource {
        resource_tag: texture.as_raw() as u64,
        view_tag: dsv.as_raw() as u64,
        descriptor: depth_lab::DsvDiscoveryDescriptor {
            view_format: view_desc.Format.0,
            view_dimension: view_desc.ViewDimension.0,
            view_flags: view_desc.Flags,
            mip_slice,
            first_array_slice,
            view_array_size,
            resource_format: desc.Format.0,
            width: desc.Width,
            height: desc.Height,
            resource_mip_levels: desc.MipLevels,
            resource_array_size: desc.ArraySize,
            sample_count: desc.SampleDesc.Count,
            sample_quality: desc.SampleDesc.Quality,
            usage: desc.Usage.0,
            bind_flags: desc.BindFlags,
            cpu_access_flags: desc.CPUAccessFlags,
            misc_flags: desc.MiscFlags,
        },
    })
}

fn record_dsv_binding(depth_stencil_view: *mut core::ffi::c_void) -> Result<(), String> {
    let _entry = if crate::dsv_discovery::enabled() {
        Some(
            crate::dsv_discovery::try_enter_observation()
                .ok_or("DSV discovery lifecycle admission is closed")?,
        )
    } else {
        None
    };
    record_dsv_binding_admitted(depth_stencil_view)
}

fn record_dsv_binding_admitted(depth_stencil_view: *mut core::ffi::c_void) -> Result<(), String> {
    let observed = if depth_stencil_view.is_null() {
        None
    } else {
        Some(inspect_dsv_resource(depth_stencil_view)?)
    };
    dsv_frame_tracker()
        .lock()
        .map_err(|_| "DSV frame tracker lock poisoned".to_string())?
        .bind(observed);
    Ok(())
}

fn record_dsv_clear(depth_stencil_view: *mut core::ffi::c_void) -> Result<(), String> {
    let _entry = if crate::dsv_discovery::enabled() {
        Some(
            crate::dsv_discovery::try_enter_observation()
                .ok_or("DSV discovery lifecycle admission is closed")?,
        )
    } else {
        None
    };
    record_dsv_clear_admitted(depth_stencil_view)
}

fn record_dsv_clear_admitted(depth_stencil_view: *mut core::ffi::c_void) -> Result<(), String> {
    let observed = inspect_dsv_resource(depth_stencil_view)?;
    dsv_frame_tracker()
        .lock()
        .map_err(|_| "DSV frame tracker lock poisoned".to_string())?
        .clear(observed);
    Ok(())
}

fn depth_stencil_view_from_generic_view(
    view: *mut core::ffi::c_void,
) -> Option<ID3D11DepthStencilView> {
    let borrowed = unsafe { ID3D11View::from_raw_borrowed(&view) }?;
    borrowed.cast().ok()
}

fn seed_dsv_tracker_from_immediate_context_admitted(
    context: *mut core::ffi::c_void,
) -> Result<(), String> {
    let borrowed = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("immediate context pointer is null while seeding DSV state")?;
    let mut dsv = None;
    unsafe { borrowed.OMGetRenderTargets(None, Some(&mut dsv)) };
    record_dsv_binding_admitted(dsv.as_ref().map_or(std::ptr::null_mut(), Interface::as_raw))
}

fn record_dsv_draw() {
    let entry = if dsv_discovery_observation_active() {
        let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
            return;
        };
        Some(entry)
    } else {
        None
    };
    // Draw 热路径免锁：镜像显示有绑定才计数，由下一次持锁入口冲账。原实现的 try_lock
    // 在 generation reset 等持锁窗口会计数丢失并作废整帧审计；原子计数没有失败模式。
    if DSV_ACTIVE_TAG.load(Ordering::Relaxed) != 0 {
        let previous = DSV_PENDING_DRAWS.fetch_add(1, Ordering::Relaxed);
        if previous == u32::MAX {
            DSV_PENDING_DRAWS.store(u32::MAX, Ordering::Relaxed);
            if let Some(entry) = &entry {
                entry.record_hook_drop();
            } else {
                crate::dsv_discovery::record_hook_drop();
            }
        }
    }
}

fn finish_dsv_selection_audit() -> Option<depth_lab::DsvSelectionAudit> {
    let (selected_resource, viewport) = DEPTH_SELECTION
        .lock()
        .ok()
        .and_then(|selection| Some((selection.selected_resource(), selection.viewport()?)))?;
    let (observations, selected_id) = dsv_frame_tracker()
        .lock()
        .ok()?
        .finish_frame(selected_resource);
    let selected_id = selected_id?;
    if observations.is_empty() {
        return None;
    }
    let ranking = depth_lab::dsv::rank(&observations, viewport);
    let audit = depth_lab::DsvSelectionAudit::from_ranking(selected_id, &ranking);
    Some(if PROFILE_DSV_OPERATOR_EXACT_PIN.load(Ordering::Acquire) {
        audit.with_operator_exact_pin()
    } else {
        audit
    })
}

fn begin_dsv_discovery_observation_after_hook_bootstrap() -> Result<bool, String> {
    if DSV_DISCOVERY_OBSERVATION_STARTED.load(Ordering::Acquire)
        || DSV_DISCOVERY_OBSERVATION_COMPLETE.load(Ordering::Acquire)
    {
        return Ok(false);
    }
    let Some(entry) = crate::dsv_discovery::try_enter_observation() else {
        return Ok(false);
    };
    let result = (|| {
        let mut tracker = dsv_frame_tracker().try_lock().map_err(
            |_| "D3D11 DSV discovery cannot establish a clean post-bootstrap frame boundary",
        )?;
        tracker.reset_for_discovery();
        drop(tracker);
        let context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
        if context == 0 {
            return Err("D3D11 DSV discovery cannot seed a missing immediate context".into());
        }
        seed_dsv_tracker_from_immediate_context_admitted(context as *mut core::ffi::c_void)
            .map_err(|error| {
                format!("D3D11 DSV discovery cannot seed the exact OM state: {error}")
            })?;
        if !DSV_DISCOVERY_DEFERRED_WARMUP_STARTED.swap(true, Ordering::AcqRel) {
            reset_deferred_dsv_ledger()?;
            log(
                "D3D11 deferred DSV ledger warmup begins; first post-install Present interval excluded",
            );
            return Ok(false);
        }
        DSV_DISCOVERY_OBSERVATION_STARTED.store(true, Ordering::Release);
        Ok(true)
    })();
    if result.is_err() {
        entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
        );
    }
    result
}

fn finish_dsv_discovery_frame(
    entry: &crate::dsv_discovery::ObservationEntryGuard,
    present_seq: u64,
) -> Option<depth_lab::DsvDiscoveryFrameObservation> {
    match dsv_frame_tracker().try_lock() {
        Ok(mut tracker) => Some(tracker.finish_discovery_frame(present_seq)),
        Err(_) => {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::TrackerFailure,
            );
            None
        }
    }
}

const fn dsv_discovery_frame_limit_reached(retained_frames: u32, limit: u32) -> bool {
    limit != 0 && retained_frames >= limit
}

fn close_dsv_discovery_observation_at_frame_limit(
    entry: &crate::dsv_discovery::ObservationEntryGuard,
) {
    let limit = DSV_DISCOVERY_FRAME_LIMIT.load(Ordering::Acquire);
    if limit == 0 {
        entry.record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::LifecycleOrQueueFailure,
        );
        DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
        DSV_DISCOVERY_OBSERVATION_COMPLETE.store(true, Ordering::Release);
        let _ = reset_deferred_dsv_ledger();
        return;
    }
    let retained = DSV_DISCOVERY_RETAINED_FRAMES.fetch_add(1, Ordering::AcqRel) + 1;
    if dsv_discovery_frame_limit_reached(retained, limit) {
        DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
        DSV_DISCOVERY_OBSERVATION_COMPLETE.store(true, Ordering::Release);
        if let Err(error) = reset_deferred_dsv_ledger() {
            entry.record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::LifecycleOrQueueFailure,
            );
            log(&format!(
                "D3D11 deferred DSV ledger release failed at observation limit: {error}"
            ));
        }
        log(&format!(
            "D3D11 DSV discovery reached its exact {limit}-frame observation limit; hook admission closed pending explicit seal"
        ));
    }
}

/// A handle existing in `OnceLock` does not prove that the live vtable slot still points at us.
/// Discovery audits every Present. An exact restoration may be rearmed so observation can
/// continue, but the known callback gap remains permanently visible in the report counters.
fn audit_dsv_discovery_hook_integrity(swapchain: *mut core::ffi::c_void) -> Result<(), String> {
    let expected_context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
    if expected_context == 0 {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
        );
        return Err("D3D11 DSV discovery has no observed immediate context".into());
    }
    // Reacquire a strong context reference from the live swapchain for every audit. Dereferencing
    // the raw context saved at hook installation would be a use-after-free if the game replaced
    // its D3D11 device/context while retaining the same process and Present hook.
    let sc = unsafe { IDXGISwapChain::from_raw_borrowed(&swapchain) }.ok_or_else(|| {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
        );
        "D3D11 DSV discovery audit swapchain pointer is invalid".to_string()
    })?;
    let device: ID3D11Device = unsafe { sc.GetDevice() }.map_err(|error| {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
        );
        format!("D3D11 DSV discovery audit GetDevice failed: {error}")
    })?;
    let context: ID3D11DeviceContext =
        unsafe { device.GetImmediateContext() }.map_err(|error| {
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
            );
            format!("D3D11 DSV discovery audit GetImmediateContext failed: {error}")
        })?;
    verify_context_inherited_aliases(&context, "game immediate discovery audit context")
        .inspect_err(|_error| {
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
            );
        })?;
    verify_immediate_context_is_runtime_unprotected(
        &context,
        "game immediate discovery audit context",
    )
    .inspect_err(|_error| {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
        );
    })?;
    audit_multithread_protection_hook(true)?;
    audit_dsv_discovery_hook_integrity_on_context(context.as_raw(), true)?;
    audit_deferred_dsv_hook_integrity(true)
}

fn audit_dsv_discovery_hook_integrity_on_context(
    context: *mut core::ffi::c_void,
    callback_gap_possible: bool,
) -> Result<(), String> {
    let _guard = CAMERA_HOOK_INTEGRITY_AUDIT_LOCK.lock().map_err(|_| {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::LifecycleOrQueueFailure,
        );
        "D3D11 DSV discovery hook integrity audit lock poisoned".to_string()
    })?;
    let expected_context = GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire);
    if context as usize != expected_context {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::ContextDrift,
        );
        return Err(format!(
            "D3D11 DSV discovery immediate context changed: expected={expected_context:#x} actual={:#x}",
            context as usize
        ));
    }
    let shadow = GAME_CONTEXT_VTABLE_SHADOW.get().ok_or_else(|| {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
        );
        "D3D11 DSV discovery game context shadow vtable missing".to_string()
    })?;
    let hooks = dsv_discovery_integrity_hooks().inspect_err(|_error| {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
        );
    })?;
    let mut failures = Vec::new();
    let mut restored = 0usize;
    match shadow.owner_state() {
        VtableOwnerState::Active => {}
        VtableOwnerState::RestoredOriginal => match unsafe { shadow.rearm_if_restored() } {
            Ok(true) => restored += 1,
            Ok(false) => {}
            Err(error) => failures.push(format!("context_vtable_owner: {error}")),
        },
        VtableOwnerState::ReplacedUnknown { current } => failures.push(format!(
            "context_vtable_owner: unknown_target backing={:#x} shadow={:#x} current={current:#x}",
            shadow.backing_vtable_address(),
            shadow.shadow_vtable_address(),
        )),
    }
    for (label, index, hook) in hooks {
        match shadow.slot_state(index, hook.original(), hook.replacement()) {
            VtableSlotState::Installed => {}
            VtableSlotState::RestoredOriginal => {
                match shadow.rearm_slot_if_restored(index, hook.original(), hook.replacement()) {
                    Ok(true) => restored += 1,
                    Ok(false) => {}
                    Err(error) => failures.push(format!("{label}: {error}")),
                }
            }
            state => failures.push(format!("{label}: {state:?}")),
        }
    }
    if callback_gap_possible {
        for _ in 0..restored {
            // A pre-Present repair proves that at least one callback interval may be missing.
            // Preserve the old per-restored-entry counter semantics and keep the report sticky.
            crate::dsv_discovery::record_integrity_failure(
                crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
            );
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        crate::dsv_discovery::record_integrity_failure(
            crate::dsv_discovery::HookIntegrityFailureReason::VtableOrSlotDrift,
        );
        Err(format!(
            "D3D11 DSV discovery hook integrity failed: {}",
            failures.join("; ")
        ))
    }
}

/// 深度采集总开关。
///
/// **名字曾经是 `DM_DEPTH_PROBE_DSV_AUDIT`**——那个名字说的是「审计」，
/// 干的却是「开启整条采集链路」。接进客户端后采集员开关的语义会和代码里的语义对不上，
/// 排障时极易误判成「审计没开」而不是「采集没开」。
///
/// 现在主名是 `DM_DEPTH_PROBE_ENABLE`；旧名**仍然认**，
/// 这样 #437 时期的脚本与 runbook 不会一夜失效。旧名命中时记一行日志提醒迁移。
/// 安装 P4 的三个 context hook。
fn install_depth_observation_hooks(ctx_raw: *mut core::ffi::c_void) -> Result<(), String> {
    if crate::camera_probe::enabled() {
        if MAP_HOOK.get().is_none() {
            verify_layout(ctx_raw, d3d11_context::MAP, "d3d11.dll")?;
            let hook = unsafe {
                VtableHook::install(
                    ctx_raw,
                    d3d11_context::MAP,
                    hooked_map as *const () as usize,
                )
                .map_err(|e| format!("安装 Map hook 失败: {e}"))?
            };
            ORIGINAL_MAP.store(hook.original(), Ordering::SeqCst);
            let _ = MAP_HOOK.set(hook);
        }
        if UNMAP_HOOK.get().is_none() {
            verify_layout(ctx_raw, d3d11_context::UNMAP, "d3d11.dll")?;
            let hook = unsafe {
                VtableHook::install(
                    ctx_raw,
                    d3d11_context::UNMAP,
                    hooked_unmap as *const () as usize,
                )
                .map_err(|e| format!("安装 Unmap hook 失败: {e}"))?
            };
            ORIGINAL_UNMAP.store(hook.original(), Ordering::SeqCst);
            let _ = UNMAP_HOOK.set(hook);
        }
        if UPDATE_SUBRESOURCE_HOOK.get().is_none() {
            verify_layout(ctx_raw, d3d11_context::UPDATE_SUBRESOURCE, "d3d11.dll")?;
            let hook = unsafe {
                VtableHook::install(
                    ctx_raw,
                    d3d11_context::UPDATE_SUBRESOURCE,
                    hooked_update_subresource as *const () as usize,
                )
                .map_err(|e| format!("安装 UpdateSubresource hook 失败: {e}"))?
            };
            ORIGINAL_UPDATE_SUBRESOURCE.store(hook.original(), Ordering::SeqCst);
            let _ = UPDATE_SUBRESOURCE_HOOK.set(hook);
        }
        if VS_SET_CONSTANT_BUFFERS_HOOK.get().is_none() {
            verify_layout(ctx_raw, d3d11_context::VS_SET_CONSTANT_BUFFERS, "d3d11.dll")?;
            let hook = unsafe {
                VtableHook::install(
                    ctx_raw,
                    d3d11_context::VS_SET_CONSTANT_BUFFERS,
                    hooked_vs_set_constant_buffers as *const () as usize,
                )
                .map_err(|e| format!("安装 VSSetConstantBuffers hook 失败: {e}"))?
            };
            ORIGINAL_VS_SET_CONSTANT_BUFFERS.store(hook.original(), Ordering::SeqCst);
            let _ = VS_SET_CONSTANT_BUFFERS_HOOK.set(hook);
        }
        if VS_SET_CONSTANT_BUFFERS1_HOOK.get().is_none()
            && let Some(context) = (unsafe { ID3D11DeviceContext::from_raw_borrowed(&ctx_raw) })
            && let Ok(context1) = context.cast::<ID3D11DeviceContext1>()
        {
            let context1_raw = context1.as_raw();
            verify_layout(
                context1_raw,
                d3d11_context::VS_SET_CONSTANT_BUFFERS1,
                "d3d11.dll",
            )?;
            let hook = unsafe {
                VtableHook::install(
                    context1_raw,
                    d3d11_context::VS_SET_CONSTANT_BUFFERS1,
                    hooked_vs_set_constant_buffers1 as *const () as usize,
                )
                .map_err(|e| format!("install VSSetConstantBuffers1 hook failed: {e}"))?
            };
            ORIGINAL_VS_SET_CONSTANT_BUFFERS1.store(hook.original(), Ordering::SeqCst);
            let _ = VS_SET_CONSTANT_BUFFERS1_HOOK.set(hook);
        }
    }
    if DRAW_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::DRAW, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW,
                hooked_draw as *const () as usize,
            )
            .map_err(|e| format!("安装 Draw hook 失败: {e}"))?
        };
        ORIGINAL_DRAW.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_HOOK.set(hook);
    }
    if DRAW_INDEXED_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::DRAW_INDEXED, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW_INDEXED,
                hooked_draw_indexed as *const () as usize,
            )
            .map_err(|e| format!("安装 DrawIndexed hook 失败: {e}"))?
        };
        ORIGINAL_DRAW_INDEXED.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_INDEXED_HOOK.set(hook);
    }
    if DRAW_INDEXED_INSTANCED_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::DRAW_INDEXED_INSTANCED, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW_INDEXED_INSTANCED,
                hooked_draw_indexed_instanced as *const () as usize,
            )
            .map_err(|e| format!("安装 DrawIndexedInstanced hook 失败: {e}"))?
        };
        ORIGINAL_DRAW_INDEXED_INSTANCED.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_INDEXED_INSTANCED_HOOK.set(hook);
    }
    if DRAW_INSTANCED_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::DRAW_INSTANCED, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW_INSTANCED,
                hooked_draw_instanced as *const () as usize,
            )
            .map_err(|e| format!("安装 DrawInstanced hook 失败: {e}"))?
        };
        ORIGINAL_DRAW_INSTANCED.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_INSTANCED_HOOK.set(hook);
    }
    if DRAW_AUTO_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::DRAW_AUTO, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW_AUTO,
                hooked_draw_auto as *const () as usize,
            )
            .map_err(|e| format!("安装 DrawAuto hook 失败: {e}"))?
        };
        ORIGINAL_DRAW_AUTO.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_AUTO_HOOK.set(hook);
    }
    if DRAW_INDEXED_INSTANCED_INDIRECT_HOOK.get().is_none() {
        verify_layout(
            ctx_raw,
            d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT,
            "d3d11.dll",
        )?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT,
                hooked_draw_indexed_instanced_indirect as *const () as usize,
            )
            .map_err(|e| format!("安装 DrawIndexedInstancedIndirect hook 失败: {e}"))?
        };
        ORIGINAL_DRAW_INDEXED_INSTANCED_INDIRECT.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_INDEXED_INSTANCED_INDIRECT_HOOK.set(hook);
    }
    if DRAW_INSTANCED_INDIRECT_HOOK.get().is_none() {
        verify_layout(ctx_raw, d3d11_context::DRAW_INSTANCED_INDIRECT, "d3d11.dll")?;
        let hook = unsafe {
            VtableHook::install(
                ctx_raw,
                d3d11_context::DRAW_INSTANCED_INDIRECT,
                hooked_draw_instanced_indirect as *const () as usize,
            )
            .map_err(|e| format!("安装 DrawInstancedIndirect hook 失败: {e}"))?
        };
        ORIGINAL_DRAW_INSTANCED_INDIRECT.store(hook.original(), Ordering::SeqCst);
        let _ = DRAW_INSTANCED_INDIRECT_HOOK.set(hook);
    }
    log("DSV pass 计数所需全部 raster draw hook 已安装");
    if crate::camera_probe::enabled() {
        log("Camera Map/Unmap/UpdateSubresource/VSSetConstantBuffers hook 已安装");
    }
    Ok(())
}

/// 这个资源是不是**常量缓冲**，以及它有多大。
///
/// 一次 `GetDesc` 同时拿到 `BindFlags` 与 `ByteWidth`——后者才是缓冲的真实长度。
/// `D3D11_MAPPED_SUBRESOURCE::RowPitch` 对缓冲资源的含义由驱动决定，**不能拿来当长度**。
fn constant_buffer_size(resource: *mut core::ffi::c_void) -> Option<usize> {
    let res = unsafe { ID3D11Resource::from_raw_borrowed(&resource) }?;
    let buffer: ID3D11Buffer = res.cast().ok()?;
    let mut desc = D3D11_BUFFER_DESC::default();
    unsafe { buffer.GetDesc(&mut desc) };
    if desc.BindFlags & D3D11_BIND_CONSTANT_BUFFER.0 as u32 == 0 {
        return None;
    }
    Some(desc.ByteWidth as usize)
}

fn snapshot_constant_buffer(
    context: *mut core::ffi::c_void,
    resource: *mut core::ffi::c_void,
    expected_bytes: usize,
) -> Result<Vec<u8>, String> {
    let source_resource = unsafe { ID3D11Resource::from_raw_borrowed(&resource) }
        .ok_or("constant-buffer snapshot source pointer is null")?;
    let source: ID3D11Buffer = source_resource
        .cast()
        .map_err(|error| format!("constant-buffer snapshot source cast failed: {error}"))?;
    let mut source_desc = D3D11_BUFFER_DESC::default();
    unsafe { source.GetDesc(&mut source_desc) };
    if source_desc.BindFlags & D3D11_BIND_CONSTANT_BUFFER.0 as u32 == 0
        || source_desc.ByteWidth as usize != expected_bytes
    {
        return Err(format!(
            "constant-buffer snapshot identity mismatch: bind_flags={:#x} bytes={} expected={expected_bytes}",
            source_desc.BindFlags, source_desc.ByteWidth
        ));
    }
    let device: ID3D11Device = unsafe { source.GetDevice() }
        .map_err(|error| format!("constant-buffer snapshot GetDevice failed: {error}"))?;
    let staging_desc = D3D11_BUFFER_DESC {
        ByteWidth: source_desc.ByteWidth,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        StructureByteStride: source_desc.StructureByteStride,
    };
    let mut staging = None;
    unsafe { device.CreateBuffer(&staging_desc, None, Some(&mut staging)) }
        .map_err(|error| format!("constant-buffer snapshot CreateBuffer failed: {error}"))?;
    let staging = staging.ok_or("constant-buffer snapshot CreateBuffer returned null")?;
    let context = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("constant-buffer snapshot context pointer is null")?;
    let destination: ID3D11Resource = staging.clone().into();
    let source_resource: ID3D11Resource = source.into();
    unsafe { context.CopyResource(&destination, &source_resource) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&destination, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|error| format!("constant-buffer snapshot Map failed: {error}"))?;
    if mapped.pData.is_null() {
        unsafe { context.Unmap(&destination, 0) };
        return Err("constant-buffer snapshot Map returned null data".into());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), expected_bytes).to_vec() };
    unsafe { context.Unmap(&destination, 0) };
    Ok(bytes)
}

fn snapshot_constant_buffer_range(
    context: *mut core::ffi::c_void,
    resource: *mut core::ffi::c_void,
    expected_resource_bytes: usize,
    offset_bytes: usize,
    range_bytes: usize,
) -> Result<Vec<u8>, String> {
    let range_end = offset_bytes
        .checked_add(range_bytes)
        .ok_or("constant-buffer range overflows usize")?;
    if range_bytes < 64 || range_end > expected_resource_bytes {
        return Err(format!(
            "constant-buffer range is invalid: resource={expected_resource_bytes} offset={offset_bytes} bytes={range_bytes}"
        ));
    }
    let source_resource = unsafe { ID3D11Resource::from_raw_borrowed(&resource) }
        .ok_or("constant-buffer range source pointer is null")?;
    let source: ID3D11Buffer = source_resource
        .cast()
        .map_err(|error| format!("constant-buffer range source cast failed: {error}"))?;
    let mut source_desc = D3D11_BUFFER_DESC::default();
    unsafe { source.GetDesc(&mut source_desc) };
    if source_desc.BindFlags & D3D11_BIND_CONSTANT_BUFFER.0 as u32 == 0
        || source_desc.ByteWidth as usize != expected_resource_bytes
    {
        return Err(format!(
            "constant-buffer range identity mismatch: bind_flags={:#x} bytes={} expected={expected_resource_bytes}",
            source_desc.BindFlags, source_desc.ByteWidth
        ));
    }
    let byte_width = u32::try_from(range_bytes)
        .map_err(|_| "constant-buffer range exceeds D3D11 buffer size".to_string())?;
    let left = u32::try_from(offset_bytes)
        .map_err(|_| "constant-buffer range offset exceeds u32".to_string())?;
    let right = u32::try_from(range_end)
        .map_err(|_| "constant-buffer range end exceeds u32".to_string())?;
    let device: ID3D11Device = unsafe { source.GetDevice() }
        .map_err(|error| format!("constant-buffer range GetDevice failed: {error}"))?;
    let staging_desc = D3D11_BUFFER_DESC {
        ByteWidth: byte_width,
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        StructureByteStride: source_desc.StructureByteStride,
    };
    let mut staging = None;
    unsafe { device.CreateBuffer(&staging_desc, None, Some(&mut staging)) }
        .map_err(|error| format!("constant-buffer range CreateBuffer failed: {error}"))?;
    let staging = staging.ok_or("constant-buffer range CreateBuffer returned null")?;
    let context = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("constant-buffer range context pointer is null")?;
    let destination: ID3D11Resource = staging.clone().into();
    let source_resource: ID3D11Resource = source.into();
    let source_box = D3D11_BOX {
        left,
        top: 0,
        front: 0,
        right,
        bottom: 1,
        back: 1,
    };
    unsafe {
        context.CopySubresourceRegion(
            &destination,
            0,
            0,
            0,
            0,
            &source_resource,
            0,
            Some(&source_box),
        )
    };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(&destination, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }
        .map_err(|error| format!("constant-buffer range Map failed: {error}"))?;
    if mapped.pData.is_null() {
        unsafe { context.Unmap(&destination, 0) };
        return Err("constant-buffer range Map returned null data".into());
    }
    let bytes =
        unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), range_bytes).to_vec() };
    unsafe { context.Unmap(&destination, 0) };
    Ok(bytes)
}

fn inspect_constant_buffer_if<T>(
    subresource: u32,
    payload_valid: bool,
    context_matches: bool,
    sampling_active: bool,
    inspect: impl FnOnce() -> Option<T>,
) -> Option<T> {
    (subresource == 0 && payload_valid && context_matches && sampling_active)
        .then(inspect)
        .flatten()
}

/// P4 现在该不该采样。
///
/// **两个条件缺一不可**，而且顺序是从便宜到贵：
/// 1. P4 开着（一个原子读）；
/// 2. production 只保存绑定快照；discovery 保存最近字节，但只在受审 draw 窗口扫描。
const fn depth_evidence_gate(
    depth_enabled: bool,
    discovery_enabled: bool,
    formal_armed: bool,
    capture_this_frame: bool,
    capture_budget_available: bool,
) -> bool {
    depth_enabled
        && (discovery_enabled || !formal_armed || (capture_this_frame && capture_budget_available))
}

fn depth_evidence_frame_active() -> bool {
    let selected_present_enabled = SELECTED_PRESENT_ENABLED.load(Ordering::Acquire);
    let selected_present_accepting = selected_present_accepting_candidates();
    depth_evidence_gate(
        depth_capture_enabled(),
        crate::camera_probe::discovery_enabled(),
        formal_capture_armed(),
        depth_candidate_capture_requested_now(),
        selected_present_accepting
            || (!selected_present_enabled
                && capture_budget_available(
                    CAPTURE_ISSUED.load(Ordering::Relaxed),
                    capture_max_frames(),
                )),
    )
}

fn render_evidence_frame_active() -> bool {
    depth_evidence_frame_active()
        || (depth_capture_enabled()
            && crate::camera_probe::production_enabled()
            && !crate::camera_probe::production_can_hold_projection())
}

fn dsv_observation_frame_active() -> bool {
    dsv_discovery_observation_active() || render_evidence_frame_active()
}

fn dsv_discovery_observation_active() -> bool {
    crate::dsv_discovery::enabled() && DSV_DISCOVERY_OBSERVATION_STARTED.load(Ordering::Acquire)
}

fn deferred_dsv_ledger_capture_active() -> bool {
    crate::dsv_discovery::enabled()
        && DEFERRED_DSV_HOOKS_READY.load(Ordering::Acquire)
        && !DSV_DISCOVERY_OBSERVATION_COMPLETE.load(Ordering::Acquire)
        && (DSV_DISCOVERY_DEFERRED_WARMUP_STARTED.load(Ordering::Acquire)
            || DSV_DISCOVERY_OBSERVATION_STARTED.load(Ordering::Acquire))
}

fn camera_sampling_active() -> bool {
    if !crate::camera_probe::enabled() {
        return false;
    }
    if crate::camera_probe::discovery_enabled() {
        return depth_capture_enabled();
    }
    render_evidence_frame_active()
}

const fn camera_deep_hook_gate(
    sampling_active: bool,
    discovery_enabled: bool,
    projection_observed_this_frame: bool,
) -> bool {
    sampling_active && (discovery_enabled || !projection_observed_this_frame)
}

fn camera_deep_sampling_active(present_seq: u64) -> bool {
    camera_deep_hook_gate(
        camera_sampling_active(),
        crate::camera_probe::discovery_enabled(),
        crate::camera_probe::production_projection_observed_this_frame(present_seq),
    )
}

fn game_context_matches(context: *mut core::ffi::c_void) -> bool {
    GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire) == context as usize
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CameraDrawEntryRoute {
    Accept,
    ForeignContext,
    HookIntegrityFailed,
}

fn camera_draw_entry_route(
    expected_context: usize,
    actual_context: usize,
    camera_enabled: bool,
    hook_integrity_failed: bool,
) -> CameraDrawEntryRoute {
    if expected_context != actual_context {
        CameraDrawEntryRoute::ForeignContext
    } else if camera_enabled && hook_integrity_failed {
        CameraDrawEntryRoute::HookIntegrityFailed
    } else {
        CameraDrawEntryRoute::Accept
    }
}

unsafe extern "system" fn hooked_vs_set_constant_buffers(
    context: *mut core::ffi::c_void,
    start_slot: u32,
    num_buffers: u32,
    buffers: *const *mut core::ffi::c_void,
) {
    crate::perf::count(crate::perf::Counter::VsSet);
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let original = ORIGINAL_VS_SET_CONSTANT_BUFFERS.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    type VsSetConstantBuffersFn =
        unsafe extern "system" fn(*mut core::ffi::c_void, u32, u32, *const *mut core::ffi::c_void);
    let original: VsSetConstantBuffersFn = unsafe { core::mem::transmute(original) };
    unsafe { original(context, start_slot, num_buffers, buffers) };

    let present_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    if !game_context_matches(context) || !camera_deep_sampling_active(present_seq) {
        return;
    }
    let _perf = crate::perf::scope(crate::perf::Section::VsSet);
    // SAFETY: D3D11 契约要求调用方提供 `num_buffers` 个指针；这里只读取目标
    // slot（production）或逐槽更新 discovery，不再为每次调用分配 Vec。
    let buffer_at = |offset: u32| -> Option<u64> {
        if buffers.is_null() || offset >= num_buffers {
            return None;
        }
        let buffer = unsafe { *buffers.add(offset as usize) };
        (!buffer.is_null()).then_some(buffer as u64)
    };
    crate::camera_probe::on_vs_constant_buffers(
        callback_generation,
        present_seq,
        start_slot,
        num_buffers,
        buffer_at,
    );
    // 冷启动：**不可变**常量缓冲的最后一次上传早于 hook，Map/Update 回调永远不会来，
    // `on_vs_constant_buffers` 的 `cached_production_buffer` seeding 也就永远拿不到字节。
    // 这是唯一需要 GPU 回读的场景。`production_buffer_snapshot_request` 只在该 tag
    // 尚未被 `known_production_buffer` 记住时返回 Some，所以整条链每个 buffer 至多走一次
    // ——不是逐调用开销，#665 省下来的常驻成本不会被它吃回去。
    if let Some((offset, buffer_tag, expected_bytes)) =
        crate::camera_probe::production_buffer_snapshot_request(start_slot, num_buffers, buffer_at)
        && !buffers.is_null()
    {
        let resource = unsafe { *buffers.add(offset as usize) };
        match snapshot_constant_buffer(context, resource, expected_bytes) {
            Ok(bytes) => crate::camera_probe::on_production_buffer_snapshot(
                callback_generation,
                buffer_tag,
                &bytes,
            ),
            Err(error) => log(&format!(
                "cold-start projection buffer snapshot failed for tag={buffer_tag:#x}: {error}"
            )),
        }
    }
}

unsafe extern "system" fn hooked_vs_set_constant_buffers1(
    context: *mut core::ffi::c_void,
    start_slot: u32,
    num_buffers: u32,
    buffers: *const *mut core::ffi::c_void,
    first_constants: *const u32,
    num_constants: *const u32,
) {
    crate::perf::count(crate::perf::Counter::VsSet);
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let original = ORIGINAL_VS_SET_CONSTANT_BUFFERS1.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    type VsSetConstantBuffers1Fn = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        u32,
        u32,
        *const *mut core::ffi::c_void,
        *const u32,
        *const u32,
    );
    let original: VsSetConstantBuffers1Fn = unsafe { core::mem::transmute(original) };
    unsafe {
        original(
            context,
            start_slot,
            num_buffers,
            buffers,
            first_constants,
            num_constants,
        )
    };

    let present_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    if !game_context_matches(context) || !camera_deep_sampling_active(present_seq) {
        return;
    }
    let _perf = crate::perf::scope(crate::perf::Section::VsSet);
    let buffer_at = |offset: u32| -> Option<u64> {
        if buffers.is_null() || offset >= num_buffers {
            return None;
        }
        let buffer = unsafe { *buffers.add(offset as usize) };
        (!buffer.is_null()).then_some(buffer as u64)
    };
    crate::camera_probe::on_vs_constant_buffers(
        callback_generation,
        present_seq,
        start_slot,
        num_buffers,
        buffer_at,
    );

    let Some(epoch) = crate::camera_probe::discovery_bound_range_snapshot_epoch(present_seq) else {
        return;
    };
    const CONSTANT_BYTES: usize = 16;
    const DEFAULT_CONSTANTS: usize = 4_096;
    let mut snapshots = Vec::new();
    for offset in 0..num_buffers {
        let Some(buffer_tag) = buffer_at(offset) else {
            continue;
        };
        let resource = buffer_tag as *mut core::ffi::c_void;
        let Some(resource_bytes) = constant_buffer_size(resource) else {
            continue;
        };
        let first = if first_constants.is_null() {
            0
        } else {
            unsafe { *first_constants.add(offset as usize) as usize }
        };
        let constants = if num_constants.is_null() {
            DEFAULT_CONSTANTS
        } else {
            unsafe { *num_constants.add(offset as usize) as usize }
        };
        let Some(range_offset) = first.checked_mul(CONSTANT_BYTES) else {
            continue;
        };
        let Some(range_bytes) = constants.checked_mul(CONSTANT_BYTES) else {
            continue;
        };
        match snapshot_constant_buffer_range(
            context,
            resource,
            resource_bytes,
            range_offset,
            range_bytes.min(resource_bytes.saturating_sub(range_offset)),
        ) {
            Ok(bytes) => snapshots.push((start_slot + offset, buffer_tag, bytes)),
            Err(error) => log(&format!(
                "camera discovery D3D11.1 range snapshot failed slot={}: {error}",
                start_slot + offset
            )),
        }
    }
    if !snapshots.is_empty()
        && let Err(error) = crate::camera_probe::on_discovery_cold_seed(epoch, &snapshots)
    {
        log(&format!(
            "camera discovery D3D11.1 range submission failed: {error}"
        ));
    }
}

unsafe extern "system" fn hooked_draw(
    context: *mut core::ffi::c_void,
    vertex_count: u32,
    start_vertex_location: u32,
) {
    observe_camera_draw_entry(CameraDrawMethod::Draw, context);
    let original = ORIGINAL_DRAW.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawFn = unsafe { core::mem::transmute(original) };
    unsafe { original(context, vertex_count, start_vertex_location) };
}

unsafe extern "system" fn hooked_draw_indexed(
    context: *mut core::ffi::c_void,
    index_count: u32,
    start_index_location: u32,
    base_vertex_location: i32,
) {
    observe_camera_draw_entry(CameraDrawMethod::DrawIndexed, context);
    let original = ORIGINAL_DRAW_INDEXED.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawIndexedFn = unsafe { core::mem::transmute(original) };
    unsafe {
        original(
            context,
            index_count,
            start_index_location,
            base_vertex_location,
        )
    };
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hooked_draw_indexed_instanced(
    context: *mut core::ffi::c_void,
    index_count_per_instance: u32,
    instance_count: u32,
    start_index_location: u32,
    base_vertex_location: i32,
    start_instance_location: u32,
) {
    observe_camera_draw_entry(CameraDrawMethod::DrawIndexedInstanced, context);
    let original = ORIGINAL_DRAW_INDEXED_INSTANCED.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawIndexedInstancedFn = unsafe { core::mem::transmute(original) };
    unsafe {
        original(
            context,
            index_count_per_instance,
            instance_count,
            start_index_location,
            base_vertex_location,
            start_instance_location,
        )
    };
}

unsafe extern "system" fn hooked_draw_instanced(
    context: *mut core::ffi::c_void,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex_location: u32,
    start_instance_location: u32,
) {
    observe_camera_draw_entry(CameraDrawMethod::DrawInstanced, context);
    let original = ORIGINAL_DRAW_INSTANCED.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawInstancedFn = unsafe { core::mem::transmute(original) };
    unsafe {
        original(
            context,
            vertex_count_per_instance,
            instance_count,
            start_vertex_location,
            start_instance_location,
        )
    };
}

unsafe extern "system" fn hooked_draw_auto(context: *mut core::ffi::c_void) {
    observe_camera_draw_entry(CameraDrawMethod::DrawAuto, context);
    let original = ORIGINAL_DRAW_AUTO.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawAutoFn = unsafe { core::mem::transmute(original) };
    unsafe { original(context) };
}

unsafe extern "system" fn hooked_draw_indexed_instanced_indirect(
    context: *mut core::ffi::c_void,
    buffer_for_args: *mut core::ffi::c_void,
    aligned_byte_offset_for_args: u32,
) {
    observe_camera_draw_entry(CameraDrawMethod::DrawIndexedInstancedIndirect, context);
    let original = ORIGINAL_DRAW_INDEXED_INSTANCED_INDIRECT.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawIndirectFn = unsafe { core::mem::transmute(original) };
    unsafe { original(context, buffer_for_args, aligned_byte_offset_for_args) };
}

unsafe extern "system" fn hooked_draw_instanced_indirect(
    context: *mut core::ffi::c_void,
    buffer_for_args: *mut core::ffi::c_void,
    aligned_byte_offset_for_args: u32,
) {
    observe_camera_draw_entry(CameraDrawMethod::DrawInstancedIndirect, context);
    let original = ORIGINAL_DRAW_INSTANCED_INDIRECT.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    let original: DrawIndirectFn = unsafe { core::mem::transmute(original) };
    unsafe { original(context, buffer_for_args, aligned_byte_offset_for_args) };
}

fn observe_camera_draw_entry(method: CameraDrawMethod, context: *mut core::ffi::c_void) {
    crate::perf::count(crate::perf::Counter::Draw);
    if !depth_capture_enabled() {
        return;
    }
    if !game_context_matches(context) {
        if dsv_discovery_observation_active() {
            crate::dsv_discovery::record_hook_drop();
        }
        if crate::camera_probe::discovery_enabled() {
            record_foreign_camera_draw_entry(method, context);
        }
        return;
    }
    if !dsv_observation_frame_active() {
        return;
    }
    let _perf = crate::perf::scope(crate::perf::Section::Draw);
    if dsv_discovery_observation_active() {
        record_dsv_draw();
        return;
    }
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let present_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    let main_pass_active =
        camera_main_pass_active_or_reacquire(context, callback_generation, present_seq);
    record_dsv_draw();
    if main_pass_active {
        SELECTED_PASS_DRAW_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    if !crate::camera_probe::enabled() {
        return;
    }
    let camera_enabled = crate::camera_probe::enabled();
    let discovery_enabled = crate::camera_probe::discovery_enabled();
    if !camera_deep_hook_gate(
        true,
        discovery_enabled,
        crate::camera_probe::production_projection_observed_this_frame(present_seq),
    ) {
        return;
    }
    match camera_draw_entry_route(
        GAME_IMMEDIATE_CONTEXT.load(Ordering::Acquire),
        context as usize,
        camera_enabled,
        CAMERA_HOOK_INTEGRITY_FAILED.load(Ordering::Acquire),
    ) {
        CameraDrawEntryRoute::Accept => {}
        CameraDrawEntryRoute::ForeignContext => {
            if discovery_enabled {
                record_foreign_camera_draw_entry(method, context);
            }
            return;
        }
        CameraDrawEntryRoute::HookIntegrityFailed => return,
    }
    if discovery_enabled
        && let Some(epoch) = crate::camera_probe::take_discovery_cold_seed_request(present_seq)
    {
        match seed_discovery_vs_buffers(context, epoch) {
            Ok((seeded, failed)) => log(&format!(
                "camera discovery cold-start VS snapshot seeded {seeded} buffers for epoch={epoch}; failed={failed}"
            )),
            Err(error) => log(&format!(
                "camera discovery cold-start VS snapshot failed: {error}"
            )),
        }
    }
    let projection_observed_before =
        crate::camera_probe::production_projection_observed_this_frame(present_seq);
    let dispatched = dispatch_camera_draw_after_gate_with_viewport(
        method,
        callback_generation,
        present_seq,
        current_qpc,
        main_pass_active,
        |bind_slot| {
            resolve_and_seed_production_vs_buffer(
                context,
                callback_generation,
                present_seq,
                bind_slot,
            )
        },
        || read_discovery_draw_viewport(context),
    );
    if dispatched
        && !projection_observed_before
        && crate::camera_probe::production_projection_observed_this_frame(present_seq)
        && let Err(error) = record_exact_draw_viewport(context, callback_generation, present_seq)
        && !VIEWPORT_OBSERVATION_ERROR_LOGGED.swap(true, Ordering::AcqRel)
    {
        log(&format!(
            "Camera production exact-draw viewport observation failed: {error}"
        ));
    }
}

/// 命中 production 精确 draw 时，取当前绑定的 VS 常量缓冲 tag 和 shader 可见字节。
///
/// `on_draw` 只在唯一那次 production 精确 draw 上调用本闭包（见
/// `camera_probe::on_draw` 的文档）。这里重新查询 slot/range 身份，但字节只接受该 resource
/// 在本 Present 的 Map/Unmap 或 UpdateSubresource 上传；这比 tag cache 强，也避免逐 Present
/// `Copy+Map` 强制 GPU/CPU 同步。
///
/// Production v1 只支持 CameraBinding 覆盖完整 resource 的形式。D3D11.1 大 backing resource
/// 的 logical subrange 需要复合身份和专用候选环；当前不能靠 COM tag 或旧缓存猜测，一律
/// fail closed。不可变 buffer 只允许 discovery 做一次性冷启动快照，不能冒充正式逐帧证据。
fn resolve_and_seed_production_vs_buffer(
    context: *mut core::ffi::c_void,
    _generation: u64,
    present_seq: u64,
    bind_slot: u32,
) -> Result<Option<(u64, Vec<u8>)>, String> {
    let context_ref = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or_else(|| "production target Draw context is invalid".to_string())?;
    let mut buffers: [Option<ID3D11Buffer>; 1] = [None];
    let mut first_constant = [0_u32; 1];
    let mut num_constants = [0_u32; 1];
    let context1 = context_ref.cast::<ID3D11DeviceContext1>().ok();
    if let Some(context1) = context1.as_ref() {
        unsafe {
            context1.VSGetConstantBuffers1(
                bind_slot,
                1,
                Some(buffers.as_mut_ptr()),
                Some(first_constant.as_mut_ptr()),
                Some(num_constants.as_mut_ptr()),
            )
        };
    } else {
        unsafe { context_ref.VSGetConstantBuffers(bind_slot, Some(&mut buffers)) };
    }
    let tag = buffers[0].as_ref().map(|buffer| buffer.as_raw() as u64);
    if !PRODUCTION_VS_QUERY_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "Camera production target Draw VS buffer slot={bind_slot} tag={tag:?} first_constant={} num_constants={}",
            first_constant[0], num_constants[0]
        ));
    }
    let Some(buffer) = buffers[0].as_ref() else {
        return Ok(None);
    };
    let buffer_tag = buffer.as_raw() as u64;
    let expected_bytes = crate::camera_probe::production_buffer_bytes_for_slot(bind_slot)
        .ok_or_else(|| format!("production VS slot {bind_slot} is not the configured source"))?;
    let resource = buffer.as_raw();
    let resource_bytes = constant_buffer_size(resource).unwrap_or(0);
    let range = context1.as_ref().map(|_| {
        (
            first_constant[0] as usize * D3D11_CONSTANT_BYTES,
            num_constants[0] as usize * D3D11_CONSTANT_BYTES,
        )
    });
    let (offset, bytes) = normalize_production_vs_range(expected_bytes, resource_bytes, range)?;
    if offset != 0 || bytes != resource_bytes {
        return Err(format!(
            "production v1 requires an exact whole-resource CPU upload: offset={offset} logical_bytes={bytes} resource_bytes={resource_bytes}"
        ));
    }
    let bytes = crate::camera_probe::exact_production_buffer_for_present(
        buffer_tag,
        present_seq,
        expected_bytes,
    )?;
    Ok(Some((buffer_tag, bytes)))
}

const D3D11_CONSTANT_BYTES: usize = 16;
const D3D11_LEGACY_FULL_BUFFER_CONSTANTS: usize = 4_096;
const D3D11_LEGACY_FULL_BUFFER_BYTES: usize =
    D3D11_LEGACY_FULL_BUFFER_CONSTANTS * D3D11_CONSTANT_BYTES;

/// Resolve the logical bytes reported by `VSGetConstantBuffers1` without confusing the
/// D3D11 legacy whole-buffer sentinel with a real 64 KiB range.
///
/// A buffer bound through `VSSetConstantBuffers` is reported by the D3D11.1 getter as
/// `(FirstConstant=0, NumConstants=4096)`, even when the backing resource is smaller.  That
/// sentinel is safe to normalize only when the complete backing resource is exactly the
/// CameraBinding range.  Explicit D3D11.1 subranges keep their exact offset and length; every
/// other mismatch remains fail-closed.
fn normalize_production_vs_range(
    expected_bytes: usize,
    resource_bytes: usize,
    observed_range: Option<(usize, usize)>,
) -> Result<(usize, usize), String> {
    match observed_range {
        Some((offset, bytes)) if bytes == expected_bytes => Ok((offset, bytes)),
        Some((0, D3D11_LEGACY_FULL_BUFFER_BYTES)) if resource_bytes == expected_bytes => {
            Ok((0, expected_bytes))
        }
        Some((offset, bytes)) => Err(format!(
            "production logical range mismatch: binding_bytes={expected_bytes} observed_offset={offset} observed_bytes={bytes} resource_bytes={resource_bytes}"
        )),
        None if resource_bytes == expected_bytes => Ok((0, expected_bytes)),
        None => Err(format!(
            "production constant-buffer size mismatch: binding_bytes={expected_bytes} resource_bytes={resource_bytes}"
        )),
    }
}

/// Seed VS constant buffers that were already populated and bound before discovery started.
/// This is claimed once per phase by `take_discovery_cold_seed_request`; it never runs in normal
/// production and therefore cannot turn synchronous GPU readback into a per-draw cost.
fn seed_discovery_vs_buffers(
    context: *mut core::ffi::c_void,
    epoch: u64,
) -> Result<(usize, usize), String> {
    const VS_SLOT_COUNT: usize = 14;
    const CONSTANT_BYTES: usize = 16;
    const LEGACY_CONSTANT_COUNT: usize = 4_096;
    let context_ref = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("camera discovery cold seed context is invalid")?;
    let mut buffers: [Option<ID3D11Buffer>; VS_SLOT_COUNT] = std::array::from_fn(|_| None);
    let mut first_constants = [0_u32; VS_SLOT_COUNT];
    let mut num_constants = [0_u32; VS_SLOT_COUNT];
    let context1 = context_ref.cast::<ID3D11DeviceContext1>().ok();
    if let Some(context1) = context1.as_ref() {
        unsafe {
            context1.VSGetConstantBuffers1(
                0,
                VS_SLOT_COUNT as u32,
                Some(buffers.as_mut_ptr()),
                Some(first_constants.as_mut_ptr()),
                Some(num_constants.as_mut_ptr()),
            )
        };
    } else {
        unsafe { context_ref.VSGetConstantBuffers(0, Some(&mut buffers)) };
    }

    let mut snapshots = Vec::new();
    let mut failed = 0usize;
    for (bind_slot, buffer) in buffers.iter().enumerate() {
        let Some(buffer) = buffer.as_ref() else {
            continue;
        };
        let resource = buffer.as_raw();
        let Some(expected_bytes) = constant_buffer_size(resource) else {
            continue;
        };
        let (offset_base, range_bytes) = if context1.is_some() {
            let base = first_constants[bind_slot] as usize * CONSTANT_BYTES;
            let len = num_constants[bind_slot] as usize * CONSTANT_BYTES;
            (base, len)
        } else {
            (
                0,
                expected_bytes.min(LEGACY_CONSTANT_COUNT * CONSTANT_BYTES),
            )
        };
        let Some(range_end) = offset_base.checked_add(range_bytes) else {
            failed = failed.saturating_add(1);
            log(&format!(
                "camera discovery cold seed slot={bind_slot} range overflow: first_constant={} num_constants={}",
                first_constants[bind_slot], num_constants[bind_slot]
            ));
            continue;
        };
        if range_bytes < 64 || range_end > expected_bytes {
            failed = failed.saturating_add(1);
            log(&format!(
                "camera discovery cold seed slot={bind_slot} invalid range: resource_bytes={expected_bytes} first_constant={} num_constants={}",
                first_constants[bind_slot], num_constants[bind_slot]
            ));
            continue;
        }
        match snapshot_constant_buffer_range(
            context,
            resource,
            expected_bytes,
            offset_base,
            range_bytes,
        ) {
            Ok(bytes) => {
                log(&format!(
                    "camera discovery cold seed slot={bind_slot} resource_bytes={expected_bytes} first_constant={} num_constants={} snapshot_bytes={}",
                    first_constants[bind_slot],
                    num_constants[bind_slot],
                    bytes.len()
                ));
                snapshots.push((bind_slot as u32, resource as u64, bytes));
            }
            Err(error) => {
                failed = failed.saturating_add(1);
                log(&format!(
                    "camera discovery cold seed slot={bind_slot} failed: {error}"
                ));
            }
        }
    }
    crate::camera_probe::on_discovery_cold_seed(epoch, &snapshots)?;
    Ok((snapshots.len(), failed))
}

fn validate_observed_viewport(viewport: ObservedDrawViewport) -> Result<(f64, f64), String> {
    if viewport.count != 1 {
        return Err(format!(
            "exact production draw bound {} viewports; production requires exactly one",
            viewport.count
        ));
    }
    if !viewport.min_depth.is_finite()
        || !viewport.max_depth.is_finite()
        || !(0.0..=1.0).contains(&viewport.min_depth)
        || !(0.0..=1.0).contains(&viewport.max_depth)
        || viewport.min_depth >= viewport.max_depth
    {
        return Err(format!(
            "exact production draw viewport depth range is invalid: min={} max={}",
            viewport.min_depth, viewport.max_depth
        ));
    }
    Ok((viewport.min_depth, viewport.max_depth))
}

fn submit_d3d12_camera_sample(
    sample: crate::d3d12_render_camera::ProductionCameraSample,
    selected: bool,
    accepting_candidates: bool,
) -> Result<bool, String> {
    let retain =
        d3d12_pose_needs_depth_projection(sample.depth_copied, selected, accepting_candidates);
    let generation = sample.row.identity.generation;
    let present_seq = sample.row.present_seq;
    let viewport = sample.viewport.ok_or_else(|| {
        format!("D3D12 depth frame {present_seq} lost its admitted draw viewport")
    })?;
    let bits = viewport
        .single_bits
        .ok_or("D3D12 admitted draw has no single-viewport values")?;
    let (min_depth, max_depth) = validate_observed_viewport(ObservedDrawViewport {
        generation,
        count: viewport.count,
        min_depth: f64::from(f32::from_bits(bits[4])),
        max_depth: f64::from(f32::from_bits(bits[5])),
    })?;
    let depth_lab::CameraPoseProvenance::ResolvedD3d12CameraHook {
        projection_matrix, ..
    } = &sample.row.provenance
    else {
        return Err("D3D12 camera row has no exact projection provenance".into());
    };
    let observed = depth_lab::camera_track::projection_from_matrix(projection_matrix)
        .ok_or("D3D12 camera projection is unusable")?;
    // Ready must exercise the same projection/profile/viewport validation as the first
    // depth index, without creating a readback or caching a frame with no consumer.
    resolve_observed_index_projection(generation, present_seq, observed, min_depth, max_depth)?;
    let submitted = crate::camera_probe::on_d3d12_pose(sample.row, retain)?;
    if submitted && retain {
        record_external_draw_viewport(
            generation,
            present_seq,
            viewport.count,
            min_depth,
            max_depth,
        )?;
    }
    Ok(submitted)
}

#[cfg(test)]
mod production_viewport_tests;

pub(crate) fn record_external_draw_viewport(
    generation: u64,
    present_seq: u64,
    count: u32,
    min_depth: f64,
    max_depth: f64,
) -> Result<(), String> {
    let viewport = ObservedDrawViewport {
        generation,
        count,
        min_depth,
        max_depth,
    };
    validate_observed_viewport(viewport)?;
    let mut observations = OBSERVED_DRAW_VIEWPORTS
        .lock()
        .map_err(|_| "external viewport cache poisoned")?;
    if generation == 0 || observations.contains_key(&present_seq) {
        return Err("external viewport identity is zero or duplicated".into());
    }
    observations.insert(present_seq, viewport);
    observations.retain(|seq, _| present_seq.saturating_sub(*seq) <= 120);
    Ok(())
}

fn record_exact_draw_viewport(
    context: *mut core::ffi::c_void,
    generation: u64,
    present_seq: u64,
) -> Result<(), String> {
    const MAX_VIEWPORTS: usize = D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as usize;
    let context = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("exact production draw context is invalid")?;
    let mut viewports = [D3D11_VIEWPORT::default(); MAX_VIEWPORTS];
    let mut count = D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE;
    unsafe { context.RSGetViewports(&mut count, Some(viewports.as_mut_ptr())) };
    let viewport = ObservedDrawViewport {
        generation,
        count,
        min_depth: if count == 1 {
            f64::from(viewports[0].MinDepth)
        } else {
            f64::NAN
        },
        max_depth: if count == 1 {
            f64::from(viewports[0].MaxDepth)
        } else {
            f64::NAN
        },
    };
    let mut observations = OBSERVED_DRAW_VIEWPORTS
        .lock()
        .map_err(|_| "exact-draw viewport lock poisoned".to_string())?;
    observations.insert(present_seq, viewport);
    observations.retain(|seq, _| present_seq.saturating_sub(*seq) <= 120);
    drop(observations);
    validate_observed_viewport(viewport).map(|_| ())
}

/// Only invoked after the discovery Present/pass/draw-window gate. No GPU readback, production
/// polling or previous-draw cache: these are the rasterizer values at this exact Draw entry.
fn read_discovery_draw_viewport(
    context: *mut core::ffi::c_void,
) -> Option<depth_lab::camera_discovery::DiscoveryViewport> {
    let context = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }?;
    let mut viewports = [D3D11_VIEWPORT::default();
        D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE as usize];
    let mut count = D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE;
    unsafe { context.RSGetViewports(&mut count, Some(viewports.as_mut_ptr())) };
    let viewport = viewports[0];
    let result = depth_lab::camera_discovery::DiscoveryViewport {
        count,
        x: viewport.TopLeftX,
        y: viewport.TopLeftY,
        width: viewport.Width,
        height: viewport.Height,
        min_depth: viewport.MinDepth,
        max_depth: viewport.MaxDepth,
    };
    // JSON cannot represent NaN/Inf. Missing stays explicit and cannot qualify for authoring.
    [
        result.x,
        result.y,
        result.width,
        result.height,
        result.min_depth,
        result.max_depth,
    ]
    .iter()
    .all(|value| value.is_finite())
    .then_some(result)
}

fn exact_draw_viewport_ready(generation: u64, present_seq: u64) -> bool {
    OBSERVED_DRAW_VIEWPORTS.lock().is_ok_and(|observations| {
        observations.get(&present_seq).is_some_and(|viewport| {
            viewport.generation == generation && validate_observed_viewport(*viewport).is_ok()
        })
    })
}

pub(crate) fn take_exact_draw_viewport(
    generation: u64,
    present_seq: u64,
) -> Result<(f64, f64), String> {
    let viewport = OBSERVED_DRAW_VIEWPORTS
        .lock()
        .map_err(|_| "exact-draw viewport lock poisoned".to_string())?
        .remove(&present_seq)
        .ok_or_else(|| {
            format!("depth frame {present_seq} has no same-Present exact-draw viewport observation")
        })?;
    if viewport.generation != generation {
        return Err(format!(
            "depth frame {present_seq} viewport generation mismatch: expected={generation} actual={}",
            viewport.generation
        ));
    }
    validate_observed_viewport(viewport)
}

fn apply_viewport_to_projection(
    projection: depth_lab::Projection,
    min_depth: f64,
    max_depth: f64,
) -> depth_lab::Projection {
    let range = max_depth - min_depth;
    depth_lab::Projection {
        a: min_depth + range * projection.a,
        b: range * projection.b,
        metric_scale: projection.metric_scale,
    }
}

#[cfg(test)]
pub(crate) fn dispatch_camera_draw_after_gate(
    method: CameraDrawMethod,
    generation: u64,
    present_seq: u64,
    sample_qpc: impl FnOnce() -> i64,
    main_pass_active: bool,
) -> bool {
    dispatch_camera_draw_after_gate_with_resolver(
        method,
        generation,
        present_seq,
        sample_qpc,
        main_pass_active,
        |_| Ok(None),
    )
}

#[cfg(test)]
fn dispatch_camera_draw_after_gate_with_resolver<Q, F>(
    method: CameraDrawMethod,
    generation: u64,
    present_seq: u64,
    sample_qpc: Q,
    main_pass_active: bool,
    resolve_bound_vs_buffer: F,
) -> bool
where
    Q: FnOnce() -> i64,
    F: FnOnce(u32) -> Result<Option<(u64, Vec<u8>)>, String>,
{
    dispatch_camera_draw_after_gate_with_viewport(
        method,
        generation,
        present_seq,
        sample_qpc,
        main_pass_active,
        resolve_bound_vs_buffer,
        || None,
    )
}

fn dispatch_camera_draw_after_gate_with_viewport<Q, F, V>(
    method: CameraDrawMethod,
    generation: u64,
    present_seq: u64,
    sample_qpc: Q,
    main_pass_active: bool,
    resolve_bound_vs_buffer: F,
    sample_viewport: V,
) -> bool
where
    Q: FnOnce() -> i64,
    F: FnOnce(u32) -> Result<Option<(u64, Vec<u8>)>, String>,
    V: FnOnce() -> Option<depth_lab::camera_discovery::DiscoveryViewport>,
{
    if crate::camera_probe::discovery_enabled() {
        record_camera_draw_entry(method);
    }
    if !main_pass_active {
        return false;
    }
    // 所有 Draw* 入口只汇入这一条账本路径；ordinal、调度和队列计数由 on_draw 统一推进。
    crate::camera_probe::on_draw_with_viewport(
        generation,
        present_seq,
        sample_qpc,
        resolve_bound_vs_buffer,
        sample_viewport,
    );
    true
}

/// 调用方契约：必须已通过 [`render_evidence_frame_active`] 门（见
/// [`observe_camera_draw_entry`]）。本函数在每个证据帧 Draw 上执行，只读原子镜像，
/// 不再拿任何锁——这是 1% low 证据帧预算里省出来的那部分。
fn camera_main_pass_active_or_reacquire(
    context: *mut core::ffi::c_void,
    generation: u64,
    present_seq: u64,
) -> bool {
    let binding_dirty = DSV_BINDING_DIRTY.load(Ordering::Acquire);
    let current_selected = !binding_dirty && CURRENT_SELECTED_RAW.load(Ordering::Acquire) != 0;
    let reacquire_pending = binding_dirty
        || (crate::camera_probe::discovery_enabled()
            && DISCOVERY_DSV_REACQUIRE_RESOURCE.load(Ordering::Acquire) != 0);
    camera_draw_gate_with_reacquire(current_selected, reacquire_pending, || {
        try_reacquire_selected_depth_on_draw(context, generation, present_seq)
    })
}

fn camera_draw_gate_with_reacquire(
    current_selected: bool,
    reacquire_pending: bool,
    reacquire: impl FnOnce() -> bool,
) -> bool {
    current_selected || (reacquire_pending && reacquire())
}

fn try_reacquire_selected_depth_on_draw(
    context: *mut core::ffi::c_void,
    generation: u64,
    present_seq: u64,
) -> bool {
    let discovery_guard = enter_owned_discovery_callback();
    if CAMERA_DISCOVERY_V3_EXACT_ACTIVE.load(Ordering::Acquire) && discovery_guard.is_none() {
        DSV_BINDING_DIRTY.store(true, Ordering::Release);
        return false;
    }
    let reacquire_route = match dsv_draw_reacquire_route(
        DISCOVERY_DSV_REACQUIRE_RESOURCE.load(Ordering::Acquire),
        DISCOVERY_DSV_REACQUIRE_VIEW.load(Ordering::Acquire),
    ) {
        Ok(route) => route,
        Err(error) => {
            latch_exact_depth_selection_ambiguity(discovery_guard.as_ref());
            DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
            DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            if !DISCOVERY_DSV_REACQUIRE_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                log(&format!(
                    "depth evidence Draw DSV 重获 latch 失败并永久拒绝本 run: {error}"
                ));
            }
            return false;
        }
    };
    if DSV_REACQUIRE_LAST_PRESENT.swap(present_seq, Ordering::AcqRel) == present_seq {
        return false;
    }
    let Some(context) = (unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }) else {
        return false;
    };
    let mut dsv: Option<ID3D11DepthStencilView> = None;
    unsafe { context.OMGetRenderTargets(None, Some(&mut dsv)) };
    let Some(dsv) = dsv else {
        return false;
    };
    match selected_depth_after_observed_unbind(
        dsv.as_raw(),
        reacquire_route.expected_pair(),
        discovery_guard.as_ref(),
    ) {
        Ok(Some(selected)) => {
            let pair = selected.pair();
            let resource = pair.resource_tag as usize;
            let mut current = match CURRENT_SELECTED_DEPTH.try_lock() {
                Ok(current) => current,
                Err(std::sync::TryLockError::WouldBlock) => {
                    // Another render callback owns the retained source briefly. Retry on the next
                    // eligible Draw; contention alone is not evidence corruption.
                    latch_exact_after_lock_failure(
                        SelectedDepthLockFailure::WouldBlock,
                        discovery_guard.as_ref(),
                    );
                    return false;
                }
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    drop(poisoned.into_inner());
                    CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                    DSV_BINDING_DIRTY.store(true, Ordering::Release);
                    latch_exact_after_lock_failure(
                        SelectedDepthLockFailure::Poisoned,
                        discovery_guard.as_ref(),
                    );
                    return false;
                }
            };
            if let DsvDrawReacquireRoute::PhaseResetExactPair(expected) = reacquire_route
                && (DISCOVERY_DSV_REACQUIRE_RESOURCE
                    .compare_exchange(
                        expected.resource_tag as usize,
                        0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_err()
                    || DISCOVERY_DSV_REACQUIRE_VIEW
                        .compare_exchange(
                            expected.view_tag as usize,
                            0,
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        )
                        .is_err())
            {
                DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
                DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
                DSV_BINDING_DIRTY.store(true, Ordering::Release);
                drop(current);
                latch_exact_depth_selection_ambiguity(discovery_guard.as_ref());
                log("depth evidence Draw DSV 重获 pair latch 发生并发漂移；fail-closed");
                return false;
            }
            if let Err(error) = record_dsv_binding(dsv.as_raw()) {
                DSV_BINDING_DIRTY.store(true, Ordering::Release);
                drop(current);
                latch_exact_depth_selection_ambiguity(discovery_guard.as_ref());
                log(&format!(
                    "depth evidence Draw DSV ranking 重建失败: {error}"
                ));
                return false;
            }
            let outgoing = current.as_ref().cloned();
            *current = Some(selected);
            CURRENT_SELECTED_RAW.store(resource, Ordering::Release);
            DSV_BINDING_DIRTY.store(false, Ordering::Release);
            drop(current);
            if update_retained_unbound_selected_depth(
                outgoing,
                Some(pair),
                discovery_guard.as_ref(),
            )
            .is_err()
            {
                match CURRENT_SELECTED_DEPTH.lock() {
                    Ok(mut current) => *current = None,
                    Err(poisoned) => {
                        drop(poisoned.into_inner());
                        latch_exact_after_lock_failure(
                            SelectedDepthLockFailure::Poisoned,
                            discovery_guard.as_ref(),
                        );
                    }
                }
                CURRENT_SELECTED_RAW.store(0, Ordering::Release);
                DSV_BINDING_DIRTY.store(true, Ordering::Release);
                return false;
            }
            crate::camera_probe::on_main_pass_bound(generation, present_seq);
            log(&format!(
                "depth evidence 已在 Draw 上通过 OMGetRenderTargets 重获受审主 DSV: present_seq={present_seq} resource={resource:#x}"
            ));
            true
        }
        Ok(None) => false,
        Err(error) => {
            DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
            DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
            DSV_BINDING_DIRTY.store(true, Ordering::Release);
            if !DISCOVERY_DSV_REACQUIRE_ERROR_LOGGED.swap(true, Ordering::AcqRel) {
                log(&format!(
                    "depth evidence Draw DSV 重获失败并停止本帧重试: {error}"
                ));
            }
            false
        }
    }
}

fn current_qpc() -> i64 {
    let mut qpc = 0i64;
    if unsafe { QueryPerformanceCounter(&mut qpc) }.is_err() {
        0
    } else {
        qpc
    }
}

unsafe extern "system" fn hooked_map(
    context: *mut core::ffi::c_void,
    resource: *mut core::ffi::c_void,
    subresource: u32,
    map_type: i32,
    map_flags: u32,
    mapped: *mut D3D11_MAPPED_SUBRESOURCE,
) -> i32 {
    crate::perf::count(crate::perf::Counter::Map);
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let original = ORIGINAL_MAP.load(Ordering::SeqCst);
    if original == 0 {
        return -1; // E_FAIL：拿不到原函数不能假装成功
    }
    type MapFn = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        u32,
        i32,
        u32,
        *mut D3D11_MAPPED_SUBRESOURCE,
    ) -> i32;
    let original: MapFn = unsafe { core::mem::transmute(original) };
    let hr = unsafe { original(context, resource, subresource, map_type, map_flags, mapped) };
    let context_matches = game_context_matches(context);
    if hr >= 0 && subresource == 0 && context_matches {
        crate::camera_probe::mark_production_buffer_write(resource as u64);
    }
    let present_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    let sampling_active = camera_deep_sampling_active(present_seq);
    let _perf =
        (hr >= 0 && subresource == 0 && !mapped.is_null() && context_matches && sampling_active)
            .then(|| crate::perf::scope(crate::perf::Section::Map));

    // 只在 Map 成功、且是主场景 pass 的常量缓冲时记录。
    // subresource != 0 的缓冲不存在，顺手排掉。
    if hr >= 0
        && let Some(size) = inspect_constant_buffer_if(
            subresource,
            !mapped.is_null(),
            context_matches,
            sampling_active,
            || constant_buffer_size(resource),
        )
    {
        let ptr = unsafe { (*mapped).pData } as *mut u8;
        unsafe {
            crate::camera_probe::on_map(
                callback_generation,
                resource as u64,
                ptr,
                size,
                present_seq,
                true,
            )
        };
    }
    hr
}

unsafe extern "system" fn hooked_unmap(
    context: *mut core::ffi::c_void,
    resource: *mut core::ffi::c_void,
    subresource: u32,
) {
    crate::perf::count(crate::perf::Counter::Map);
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    // **必须在原始 Unmap 之前读**——之后那块内存就不属于我们了。
    if subresource == 0 && game_context_matches(context) && camera_sampling_active() {
        let _perf = crate::perf::scope(crate::perf::Section::Map);
        unsafe { crate::camera_probe::on_unmap(callback_generation, resource as u64) };
    }
    let original = ORIGINAL_UNMAP.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    type UnmapFn = unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u32);
    let original: UnmapFn = unsafe { core::mem::transmute(original) };
    unsafe { original(context, resource, subresource) };
}

#[allow(clippy::too_many_arguments)]
unsafe extern "system" fn hooked_update_subresource(
    context: *mut core::ffi::c_void,
    resource: *mut core::ffi::c_void,
    subresource: u32,
    dst_box: *const core::ffi::c_void,
    src_data: *const core::ffi::c_void,
    src_row_pitch: u32,
    src_depth_pitch: u32,
) {
    crate::perf::count(crate::perf::Counter::Map);
    let callback_generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    let context_matches = game_context_matches(context);
    if subresource == 0 && context_matches {
        crate::camera_probe::mark_production_buffer_write(resource as u64);
    }
    let present_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    let sampling_active = camera_deep_sampling_active(present_seq);
    let payload_valid = dst_box.is_null() && !src_data.is_null();
    let _perf = (subresource == 0 && payload_valid && context_matches && sampling_active)
        .then(|| crate::perf::scope(crate::perf::Section::Map));
    if let Some(size) = inspect_constant_buffer_if(
        subresource,
        payload_valid,
        context_matches,
        sampling_active,
        || constant_buffer_size(resource),
    ) {
        // dst_box 非空表示只更新缓冲的一段，此时 src_data 覆盖的长度不是 ByteWidth。
        // 与其猜，不如跳过——部分更新的常量缓冲不是相机矩阵的常见上传方式。
        unsafe {
            crate::camera_probe::on_update_subresource(
                callback_generation,
                resource as u64,
                src_data as *const u8,
                size,
                present_seq,
                true,
            )
        };
    }
    let original = ORIGINAL_UPDATE_SUBRESOURCE.load(Ordering::SeqCst);
    if original == 0 {
        return;
    }
    type UpdateFn = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        *mut core::ffi::c_void,
        u32,
        *const core::ffi::c_void,
        *const core::ffi::c_void,
        u32,
        u32,
    );
    let original: UpdateFn = unsafe { core::mem::transmute(original) };
    unsafe {
        original(
            context,
            resource,
            subresource,
            dst_box,
            src_data,
            src_row_pitch,
            src_depth_pitch,
        )
    };
}

/// 把当前线程降到 Windows 后台调度模式（CPU + I/O 同时降）。
///
/// 深度与 P4 都是**旁路采集**，绝不能和游戏平级抢资源。机器忙时让有界队列丢样本，
/// 也比把渲染线程饿到掉帧更符合性能契约——掉帧会同时毁掉 RGB 那一半（R-1）。
pub(crate) fn set_current_thread_background() {
    if let Err(error) =
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) }
    {
        log(&format!("设置后台线程优先级失败: {error}"));
    }
}

/// Formal lossless writers stay below the game's CPU priority but retain normal I/O priority.
/// `THREAD_MODE_BACKGROUND_BEGIN` also lowers Windows I/O priority, which lets a normal-priority
/// segment hash starve a bounded Camera/depth queue at rollover even when CPU work is tiny.
pub(crate) fn set_current_thread_lossless_writer_priority() {
    if let Err(error) =
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL) }
    {
        log(&format!("设置无损 writer 线程优先级失败: {error}"));
    }
}

/// 本次采集的投影参数与 Z 约定，**每个 StartRun 从 depth profile 读一次**。
///
/// # 为什么不再硬编码 `ZConvention::Traditional`
///
/// 那个硬编码是当初为 BioShock Infinite（UE3，传统 Z）写的。切到 ETS2（reversed-Z）
/// 之后没人改，而 `Traditional => min` 在 reversed-Z 下取的是**最远的表面**——
/// 1920×1080 → 1280×720 每个 1.5×1.5 块都系统性地吃掉前景边缘，
/// **而整帧看起来仍然像一张正常的深度图**。
///
/// 现在它由 `projection_b` 的符号推出（`d = a + b/z` ⇒ `dd/dz = −b/z²`），
/// 与线性化用的是同一个真值来源，不可能互相矛盾。
///
/// profile 缺失时返回 `None`：**降采样方向未知就不该采**，采了也是错的。

#[derive(Clone, Copy)]
struct ResolvedProjection {
    conv: ZConvention,
    index: depth_lab::capture_timing::IndexProjection,
    readback_slots: usize,
}

#[derive(Clone, Copy)]
struct RunProjection {
    generation: u64,
    resolved: Option<ResolvedProjection>,
    allow_unvalidated_projection_family: bool,
    unvalidated_projection_family_logged: bool,
}

struct RunProjectionState {
    active: Option<RunProjection>,
}

impl RunProjectionState {
    const fn new() -> Self {
        Self { active: None }
    }

    fn start(
        &mut self,
        generation: u64,
        resolved: Option<ResolvedProjection>,
        allow_unvalidated_projection_family: bool,
    ) -> Result<(), String> {
        if let Some(active) = self.active {
            return Err(format!(
                "previous projection generation {} is still active",
                active.generation
            ));
        }
        self.active = Some(RunProjection {
            generation,
            resolved,
            allow_unvalidated_projection_family,
            unvalidated_projection_family_logged: false,
        });
        Ok(())
    }

    fn get(&self, generation: u64) -> Result<Option<ResolvedProjection>, String> {
        let active = self
            .active
            .ok_or_else(|| "run projection is not initialized".to_string())?;
        if active.generation != generation {
            return Err(format!(
                "run projection generation mismatch: requested={generation} active={}",
                active.generation
            ));
        }
        Ok(active.resolved)
    }

    fn accept_unvalidated_projection_family(
        &mut self,
        generation: u64,
    ) -> Result<Option<bool>, String> {
        let active = self
            .active
            .as_mut()
            .ok_or_else(|| "run projection is not initialized".to_string())?;
        if active.generation != generation {
            return Err(format!(
                "run projection generation mismatch: requested={generation} active={}",
                active.generation
            ));
        }
        if !active.allow_unvalidated_projection_family {
            return Ok(None);
        }
        let first_observation = !active.unvalidated_projection_family_logged;
        active.unvalidated_projection_family_logged = true;
        Ok(Some(first_observation))
    }

    fn stop(&mut self, generation: u64) -> Result<(), String> {
        let active = self
            .active
            .ok_or_else(|| "run projection is not initialized during stop".to_string())?;
        if active.generation != generation {
            return Err(format!(
                "run projection stop generation mismatch: requested={generation} active={}",
                active.generation
            ));
        }
        self.active = None;
        Ok(())
    }
}

static ACTIVE_PROJECTION: Mutex<RunProjectionState> = Mutex::new(RunProjectionState::new());

fn resolve_projection_for_run(
    config: &depth_lab::probe_config::ProbeConfig,
) -> Option<ResolvedProjection> {
    let Some(path) = config.depth_profile_path.as_deref() else {
        log("配置未给 depth_profile_path：投影参数与 Z 约定均未知。\
             深度仍会落盘，但索引会标 proj_source=unknown，事后无法线性化成米（#490）");
        return None;
    };
    match load_depth_profile(
        std::path::Path::new(path),
        config.depth_profile_sha256.as_deref(),
    ) {
        Ok(resolved) => {
            log(&format!(
                "depth profile 已按当前 run 加载：a={:?} b={:?} reverse_z={:?} source={:?}",
                resolved.index.proj_a,
                resolved.index.proj_b,
                resolved.index.reverse_z,
                resolved.index.proj_source
            ));
            Some(resolved)
        }
        Err(error) => {
            log(&format!("depth profile 读取失败（{path}）: {error}"));
            None
        }
    }
}

fn active_projection(generation: u64) -> Result<Option<ResolvedProjection>, String> {
    ACTIVE_PROJECTION
        .lock()
        .map_err(|_| "run projection lock poisoned".to_string())?
        .get(generation)
}

fn accept_unvalidated_projection_family(generation: u64) -> Result<Option<bool>, String> {
    ACTIVE_PROJECTION
        .lock()
        .map_err(|_| "run projection lock poisoned during observation".to_string())?
        .accept_unvalidated_projection_family(generation)
}

fn stop_active_projection(generation: u64) -> Result<(), String> {
    ACTIVE_PROJECTION
        .lock()
        .map_err(|_| "run projection lock poisoned during stop".to_string())?
        .stop(generation)
}

const OBSERVED_PROJECTION_RELATIVE_TOLERANCE: f64 = 1.0e-6;
const OBSERVED_PROJECTION_ABSOLUTE_TOLERANCE: f64 = 1.0e-8;

fn projection_coefficient_matches(expected: f64, observed: f64) -> bool {
    expected.is_finite()
        && observed.is_finite()
        && (expected - observed).abs()
            <= OBSERVED_PROJECTION_ABSOLUTE_TOLERANCE
                + OBSERVED_PROJECTION_RELATIVE_TOLERANCE * expected.abs().max(observed.abs())
}

fn projection_clip_planes(
    projection: depth_lab::Projection,
) -> Option<(ZConvention, f64, Option<f64>)> {
    if !projection.a.is_finite() || !projection.b.is_finite() || projection.b == 0.0 {
        return None;
    }
    let convention = projection.z_convention();
    let (near, far) = match convention {
        ZConvention::Traditional => (
            -projection.b / projection.a,
            (projection.a != 1.0).then(|| -projection.b / (projection.a - 1.0)),
        ),
        ZConvention::Reversed => (
            projection.b / (1.0 - projection.a),
            (projection.a != 0.0).then(|| -projection.b / projection.a),
        ),
    };
    if !near.is_finite() || near <= 0.0 {
        return None;
    }
    if let Some(far) = far
        && (!far.is_finite() || far <= near)
    {
        return None;
    }
    Some((convention, near, far))
}

/// A validated exact draw may legitimately change its far clip plane between an exterior and an
/// interior cell.  The same-Present matrix remains authoritative for linearization.  The static
/// profile therefore authorizes the projection family (Z convention and near plane), while the
/// exact CameraBinding draw/slot/range identity protects the source itself.
fn projection_family_matches(
    expected: depth_lab::Projection,
    observed: depth_lab::Projection,
) -> bool {
    let Some((expected_convention, expected_near, _)) = projection_clip_planes(expected) else {
        return false;
    };
    let Some((observed_convention, observed_near, _)) = projection_clip_planes(observed) else {
        return false;
    };
    expected_convention == observed_convention
        && projection_coefficient_matches(expected_near, observed_near)
}

fn observed_index_projection(
    generation: u64,
    present_seq: u64,
) -> Result<depth_lab::capture_timing::IndexProjection, String> {
    let observed = crate::camera_probe::take_observed_projection(generation, present_seq)?
        .ok_or_else(|| {
            format!(
                "depth frame {present_seq} has no same-Present production projection observation"
            )
        })?;
    let (min_depth, max_depth) = take_exact_draw_viewport(generation, present_seq)?;
    resolve_observed_index_projection(generation, present_seq, observed, min_depth, max_depth)
}

fn resolve_observed_index_projection(
    generation: u64,
    present_seq: u64,
    observed: depth_lab::Projection,
    viewport_min_depth: f64,
    viewport_max_depth: f64,
) -> Result<depth_lab::capture_timing::IndexProjection, String> {
    use depth_lab::capture_timing::IndexProjectionSource;

    let reference = active_projection(generation)?
        .ok_or_else(|| "production depth frame has no validated projection profile".to_string())?;
    if matches!(
        reference.index.proj_source,
        IndexProjectionSource::Candidate | IndexProjectionSource::Unknown
    ) {
        return Err(format!(
            "production depth frame cannot promote {:?} profile evidence to observed",
            reference.index.proj_source
        ));
    }
    let expected_a = reference
        .index
        .proj_a
        .ok_or_else(|| "validated profile is missing projection A".to_string())?;
    let expected_b = reference
        .index
        .proj_b
        .ok_or_else(|| "validated profile is missing projection B".to_string())?;
    let metric_scale = reference
        .index
        .metric_scale
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .ok_or_else(|| "validated profile is missing a positive metric scale".to_string())?;
    let expected = depth_lab::Projection {
        a: expected_a,
        b: expected_b,
        metric_scale,
    };
    let observed = depth_lab::Projection {
        a: observed.a,
        b: observed.b,
        metric_scale,
    };
    if !projection_family_matches(expected, observed) {
        let mismatch = format!(
            "same-Present projection family mismatch at {present_seq}: profile=({expected_a:?},{expected_b:?}) planes={:?} observed=({:?},{:?}) planes={:?}",
            projection_clip_planes(expected),
            observed.a,
            observed.b,
            projection_clip_planes(observed),
        );
        if projection_clip_planes(observed).is_none() {
            return Err(mismatch);
        }
        let Some(first_observation) = accept_unvalidated_projection_family(generation)? else {
            return Err(mismatch);
        };
        if first_observation {
            log(&format!(
                "performance-validation-only accepted an uncertified projection family; run remains permanently non-deliverable: {mismatch}"
            ));
        }
    }
    let effective = apply_viewport_to_projection(observed, viewport_min_depth, viewport_max_depth);
    Ok(
        depth_lab::capture_timing::IndexProjection::observed_with_viewport(
            effective,
            Some(metric_scale),
            viewport_min_depth,
            viewport_max_depth,
        ),
    )
}

/// 加载投影来源：正式 profile，或**只有投影参数的候选文件**。
///
/// # 按 `schema_version` 分派，不做「先试 A 失败再试 B」
///
/// 后者会把「profile 里有个字段拼错了」静默降级成「按候选加载」——
/// 于是一份本该被 `validate()` 拦下的 profile 变成了不可交付的候选，
/// 而日志上看起来一切正常。文件说自己是什么，就按什么解析，解析失败就报错。
fn load_depth_profile(
    path: &std::path::Path,
    expected_sha256: Option<&str>,
) -> Result<ResolvedProjection, String> {
    use depth_lab::profile::{CandidateProjection, DepthProfile};

    let bytes = match expected_sha256 {
        Some(expected) => read_verified_snapshot(path, expected, "depth profile")?,
        None => std::fs::read(path).map_err(|e| format!("读 {} 失败: {e}", path.display()))?,
    };
    let text = std::str::from_utf8(&bytes).map_err(|e| format!("profile 不是 UTF-8: {e}"))?;
    let probe: serde_json::Value =
        serde_json::from_str(text).map_err(|e| format!("不是合法 JSON: {e}"))?;
    let schema = probe
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "缺 schema_version，无法判断这是 profile 还是候选投影".to_string())?;

    if schema == CandidateProjection::SCHEMA_VERSION {
        let candidate: CandidateProjection =
            serde_json::from_str(text).map_err(|e| format!("候选投影解析失败: {e}"))?;
        candidate.validate()?;
        let projection = candidate.projection();
        log(&format!(
            "加载的是**候选**投影（{}），不是已标定 profile。参数会写进索引但标 \
             proj_source=candidate，交付判定一律拒。仍缺：{}",
            candidate.game_id,
            candidate.blocking.join("；")
        ));
        return Ok(ResolvedProjection {
            conv: projection.z_convention(),
            index: depth_lab::capture_timing::IndexProjection::known(
                projection,
                Some(candidate.metric_scale),
                depth_lab::capture_timing::IndexProjectionSource::Candidate,
            ),
            readback_slots: DEFAULT_READBACK_SLOTS,
        });
    }

    let profile: DepthProfile = serde_json::from_str(text).map_err(|e| format!("解析失败: {e}"))?;
    let projection = depth_lab::depth::Projection {
        a: profile.projection_a,
        b: profile.projection_b,
        metric_scale: profile.metric_scale.value,
    };
    Ok(ResolvedProjection {
        conv: projection.z_convention(),
        index: depth_lab::capture_timing::IndexProjection::known(
            projection,
            Some(profile.metric_scale.value),
            profile.projection_source.into(),
        ),
        readback_slots: profile.readback_slots,
    })
}

/// profile 的投影参数能不能定出落盘变换（94 号）。
///
/// `reverse_z` 是由 `projection_b` 的符号**算出来**的，不是独立字段，
/// 所以这里不可能与线性化用的参数矛盾。参数未知时返回 `None`：
/// **不猜**——猜错的方向会让深度整体反过来且看起来正常。
fn transform_from_profile(
    projection: &depth_lab::capture_timing::IndexProjection,
) -> Option<depth_lab::capture_timing::StorageTransform> {
    use depth_lab::capture_timing::StorageTransform;
    projection.reverse_z.map(|rev| {
        StorageTransform::for_convention(if rev {
            ZConvention::Reversed
        } else {
            ZConvention::Traditional
        })
    })
}

fn depth_capture_enabled() -> bool {
    DEPTH_CAPTURE_ENABLED.load(Ordering::Acquire)
}

/// 本次运行是否为「仅水印」档（性能认证的中间档）。
///
/// **逐 run 装载，不是逐进程**——`begin_run` 从本轮 `ProbeConfig` 置位。
/// 每帧读盘不可接受，所以仍然只解析一次；但那个「一次」的作用域必须是 run，
/// 与 `ACTIVE_CONFIG`（`begin_run` 装、run 结束清）一致。
///
/// 用 `OnceLock` 缓存到进程生命周期会造出一个**静默错误的认证**：三档里的
/// 「仅水印」与「完整」若跑在同一个游戏进程里（DLL 常驻，`ACTIVE_CONFIG` 本就
/// 支持一进程多 run），完整档会沿用仅水印档的 `true`，于是回读整条从未被执行，
/// `readback_avg_drop` ≈ 0，认证**通过**并打开性能门——而那个数字下面没有测量。
/// 产物结构完全合规、只有数值是错的，正是 130 号 §6.4 那一类。
static PRESENT_ONLY: AtomicBool = AtomicBool::new(false);
static WATERMARK_ONLY: AtomicBool = AtomicBool::new(false);
static WRITER_BYPASS: AtomicBool = AtomicBool::new(false);

fn present_only() -> bool {
    PRESENT_ONLY.load(Ordering::Acquire)
}

/// 只写水印、不回读深度。
///
/// # 为什么是「跳过回读」而不是「关掉采集」
///
/// 四档性能门要的是**归因**：基线 / 仅水印 / 回读旁路 / 完整。
/// 所以这一档必须**保留水印的全部真实开销**（RTV 缓存、两次 ClearView、
/// ResizeBuffers hook），只把回读那一半拿掉。
///
/// 把它做成 `enabled=false` 是错的——那样连水印都不写，测出来等于基线。
fn watermark_only() -> bool {
    WATERMARK_ONLY.load(Ordering::Acquire)
}

fn writer_bypass() -> bool {
    WRITER_BYPASS.load(Ordering::Acquire)
}

fn d3d12_deep_observation_required(present_only: bool, watermark_only: bool) -> bool {
    !present_only && !watermark_only
}

fn d3d12_presentation_only() -> bool {
    D3D12_EARLY_BOOTSTRAP.get().is_some() && watermark_only()
}

/// D3D12 baseline/watermark-only stages carry a Present timeline, not a pose stream, and the
/// client accepts that empty shape only when no player stream is reported. KCD2's live-pose
/// summary always reports a count (zero in those stages), which made every KCD2 certification
/// fail its first two stages; the released generation-7 probe applied this same rule.
const fn flushed_player_pose_row_count(
    presentation_only: bool,
    reported: Option<u64>,
) -> Option<u64> {
    if presentation_only { None } else { reported }
}

fn formal_capture_armed() -> bool {
    FORMAL_CAPTURE_ARMED.load(Ordering::Acquire)
}

fn observe_capture_warmup(generation: u64, observation: impl Into<CaptureWarmupEvidence>) -> bool {
    let ready = CAPTURE_WARMUP
        .try_lock()
        .ok()
        .and_then(|mut warmup| {
            warmup
                .as_mut()
                .map(|warmup| warmup.observe(generation, Instant::now(), observation))
        })
        .unwrap_or(false);
    ready
        && (LOCAL_COMPAT_FPS.load(Ordering::Acquire) == 0
            || LOCAL_COMPAT_PASSED.load(Ordering::Acquire))
}

fn record_capture_warmup_error(error: String) {
    if let Ok(mut stored) = CAPTURE_WARMUP_ERROR.try_lock()
        && stored.is_none()
    {
        *stored = Some(error.clone());
        log(&format!("capture warmup fail-closed: {error}"));
    }
}

fn arm_formal_capture(generation: u64) -> Result<(), String> {
    if generation == 0 || ACTIVE_GENERATION.load(Ordering::Acquire) != generation {
        return Err(format!(
            "capture warmup generation mismatch: requested={generation} active={}",
            ACTIVE_GENERATION.load(Ordering::Acquire)
        ));
    }
    if CAPTURE_ISSUED.load(Ordering::Acquire) != 0 {
        return Err("capture warmup consumed formal depth budget before Ready".into());
    }
    if !d3d12_presentation_only() {
        arm_camera_production(generation)?;
    }
    D3D12_FORMAL_START_PRESENT.store(PRESENT_SEQ.load(Ordering::Acquire), Ordering::Release);
    *CADENCE
        .lock()
        .map_err(|_| "cadence lock poisoned during formal arm".to_string())? = None;
    CAPTURE_THIS_FRAME.store(false, Ordering::Release);
    FIRST_PRESENT_SEQ.store(u64::MAX, Ordering::Release);
    FORMAL_CAPTURE_ARMED.store(true, Ordering::Release);
    log(&format!(
        "capture generation={generation} warmup complete; formal cadence and Camera timeline armed"
    ));
    // 一轮录制只走一次。拿不到锁就跳过——这条纯诊断，绝不能因为它让预热失败。
    if let Ok(warmup) = CAPTURE_WARMUP.try_lock()
        && let Some(warmup) = warmup.as_ref()
    {
        log(&format!(
            "warmup_timing {}",
            warmup.completion_summary_json()
        ));
    }
    Ok(())
}

struct LoadedLegacyCaptureConfig {
    config: depth_lab::probe_config::ProbeConfig,
    /// Present only for the bounded exact bytes consumed from the managed ProgramData file.
    source_sha256: Option<String>,
}

/// 只在初始化线程调用一次。`source_sha256=Some` 表示由客户端配置文件管理，
/// 需要后台观察关闭信号；环境变量启动的手工流程不受该文件控制。
fn initialise_legacy_capture() -> Option<LoadedLegacyCaptureConfig> {
    // 优先读约定路径的配置文件。**客户端驱动的注入只能走这条路**——
    // probe 读到的 env 是**游戏进程**的环境，注入器上设的变量到不了它
    // （见 depth_lab::probe_config 的模块文档）。
    match depth_lab::probe_config::load_with_bytes() {
        Ok((cfg, bytes)) => {
            use sha2::{Digest as _, Sha256};

            log(&format!(
                "按配置文件启动：session={} output={} unbind_from_end={}",
                cfg.session_id, cfg.output_dir, cfg.unbind_from_end
            ));
            return Some(LoadedLegacyCaptureConfig {
                config: cfg,
                source_sha256: Some(format!("sha256:{:x}", Sha256::digest(&bytes))),
            });
        }
        Err(depth_lab::probe_config::ConfigError::NotPresent) => {}
        Err(e) => {
            log(&format!("配置文件存在但不可用：{e}"));
            return None;
        }
    }

    // 手工开发流程：operator 先 set 变量、再从同一个 shell 启动游戏。
    let enabled = if std::env::var_os("DM_DEPTH_PROBE_ENABLE").is_some() {
        true
    } else if std::env::var_os("DM_DEPTH_PROBE_DSV_AUDIT").is_some() {
        log("DM_DEPTH_PROBE_DSV_AUDIT 已更名为 DM_DEPTH_PROBE_ENABLE；旧名仍可用但请尽快迁移");
        true
    } else {
        false
    };
    enabled
        .then(resolved_config)
        .flatten()
        .map(|config| LoadedLegacyCaptureConfig {
            config,
            source_sha256: None,
        })
}

fn start_capture_control_watcher() -> Result<(), String> {
    std::thread::Builder::new()
        .name("dm-depth-control".into())
        .spawn(control_loop)
        .map(|_| ())
        .map_err(|e| format!("启动深度控制线程失败: {e}"))
}

fn start_legacy_config_watcher(session_id: String) -> Result<(), String> {
    let (ready_tx, ready_rx) = sync_channel::<Result<(), String>>(1);
    std::thread::Builder::new()
        .name("dm-depth-config-fallback".into())
        .spawn(move || {
            let initial = depth_lab::probe_config::load()
                .map_err(|error| format!("initial watcher config load failed: {error}"))
                .and_then(|config| {
                    if config.session_id != session_id {
                        return Err("initial watcher session identity changed".into());
                    }
                    let active = ACTIVE_CONFIG
                        .lock()
                        .map_err(|_| "active config lock poisoned during watcher readiness")?;
                    if active.as_ref() != Some(&config) {
                        return Err("initial watcher config differs from the active run".into());
                    }
                    Ok(())
                });
            let ready = initial.is_ok();
            if ready_tx.send(initial).is_err() || !ready {
                return;
            }
            while depth_capture_enabled() {
                std::thread::sleep(Duration::from_millis(250));
                let still_enabled = match depth_lab::probe_config::load() {
                    Ok(config) if config.session_id == session_id => {
                        let active = match ACTIVE_CONFIG.lock() {
                            Ok(active) => active.clone(),
                            Err(poisoned) => {
                                drop(poisoned.into_inner());
                                log("active config lock poisoned in watcher; stopping fail-closed");
                                None
                            }
                        };
                        match active {
                            Some(active) if active == config => true,
                            Some(active) if active.dsv_discovery.is_some() => {
                                log("D3D11 DSV discovery config changed; sealing instead of hot-reloading the evidence identity");
                                false
                            }
                            Some(active) => {
                                match reset_legacy_camera_discovery_phase(&active, &config) {
                                    Ok(()) => match ACTIVE_CONFIG.lock() {
                                        Ok(mut active) => {
                                            *active = Some(config.clone());
                                            log("legacy camera discovery phase 已在 Flush/Start ACK 后切换");
                                            true
                                        }
                                        Err(poisoned) => {
                                            drop(poisoned.into_inner());
                                            log("active config lock poisoned after phase reset; stopping fail-closed");
                                            false
                                        }
                                    },
                                    Err(error) => {
                                        log(&format!(
                                            "legacy camera discovery phase reset 失败并停止采集: {error}"
                                        ));
                                        false
                                    }
                                }
                            }
                            None => false,
                        }
                    }
                    _ => false,
                };
                if !still_enabled {
                    let dsv_discovery_active = crate::dsv_discovery::enabled();
                    // DSV stop and activation-ACK commit share their own lifecycle lock. Do not
                    // close the outer observation gate before that lock decides which transition
                    // wins, or an ACK could be committed after observation was already disabled.
                    FORMAL_CAPTURE_ARMED.store(false, Ordering::Release);
                    if dsv_discovery_active {
                        match crate::dsv_discovery::stop() {
                            Ok(report) => log(&format!(
                                "D3D11 DSV discovery report sealed: {}",
                                report.display()
                            )),
                            Err(error) => log(&format!(
                                "D3D11 DSV discovery report seal failed: {error}"
                            )),
                        }
                    }
                    {
                        let _present_guard = PRESENT_RUN_LOCK
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        DEPTH_CAPTURE_ENABLED.store(false, Ordering::Release);
                        if let Err(error) = clear_observed_swapchain_devices() {
                            log(&format!(
                                "legacy swapchain adapter cleanup completed with warning: {error}"
                            ));
                        }
                    }
                    DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
                    DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
                    if dsv_discovery_active {
                        if let Ok(mut tracker) = dsv_frame_tracker().lock() {
                            tracker.reset();
                        }
                        DSV_DISCOVERY_OBSERVATION_STARTED.store(false, Ordering::Release);
                        DSV_DISCOVERY_DEFERRED_WARMUP_STARTED.store(false, Ordering::Release);
                        DSV_DISCOVERY_OBSERVATION_COMPLETE.store(false, Ordering::Release);
                        DSV_DISCOVERY_RETAINED_FRAMES.store(0, Ordering::Release);
                        DSV_DISCOVERY_FRAME_LIMIT.store(0, Ordering::Release);
                        if let Err(error) = reset_deferred_dsv_ledger() {
                            log(&format!(
                                "D3D11 deferred DSV ledger release failed while stopping: {error}"
                            ));
                        }
                    } else {
                        if let Err(error) = crate::camera_probe::stop_discovery() {
                            log(&format!(
                                "legacy camera discovery 最终 Flush ACK 失败: {error}"
                            ));
                        }
                        let phase_id = ACTIVE_CONFIG
                            .lock()
                            .ok()
                            .and_then(|active| {
                                active
                                    .as_ref()
                                    .and_then(|config| config.camera_discovery.as_ref())
                                    .map(|phase| phase.phase_id.clone())
                            })
                            .unwrap_or_else(|| "unknown".to_string());
                        log_and_reset_camera_hook_entry_counters(&phase_id);
                    }
                    log(&format!(
                        "legacy session {session_id} config closed or replaced; stopped new depth capture without lifecycle ACK"
                    ));
                    break;
                }
            }
        })
        .map_err(|error| format!("start legacy depth config watcher failed: {error}"))?;
    ready_rx
        .recv()
        .map_err(|_| "legacy depth config watcher returned no readiness signal".to_string())?
}

fn reset_legacy_camera_discovery_phase(
    active: &depth_lab::probe_config::ProbeConfig,
    next: &depth_lab::probe_config::ProbeConfig,
) -> Result<(), String> {
    let discovery = camera_discovery_phase_reset_config(active, next)?;
    let replayed_descriptor = validate_camera_discovery_selection(next)?;
    let strict_v3_selection = replayed_descriptor.is_some();
    let expected = replayed_descriptor.map_or_else(
        || {
            discovery
                .render
                .clone()
                .map(DepthSelectionExpectation::LegacyRender)
                .ok_or_else(|| "legacy camera phase reset is missing render identity".to_string())
        },
        |descriptor| Ok(DepthSelectionExpectation::ExactDescriptor(descriptor)),
    )?;
    if !strict_v3_selection {
        let actual = current_executable_sha256()?;
        if discovery.executable_sha256 != actual {
            return Err(format!(
                "camera discovery executable hash 漂移: config={} actual={actual}",
                discovery.executable_sha256
            ));
        }
    }

    // run7 的七个 Draw 入口归零发生在 phase 切换前后。先在旧 phase 仍启用时审计，
    // 这样若发现槽位曾被还原，缺口会记入旧 phase；仅 original 可安全重挂，未知目标
    // 或 context/vtable 漂移直接拒绝热切。新 phase 随后还会由 Present 用 swapchain
    // 实际 GetImmediateContext 再核一次。
    audit_camera_hook_integrity_before_phase_reset()?;

    // 先关 producer，等待旧 phase drain、最终报告封口与 Flush ACK；新 phase 随后只有在
    // worker 返回 Start ACK 后才启用。保留 DSV 两把锁直到新 phase 完成初始化，避免
    // render hook 在验证与切换之间替换当前绑定。
    let _transition_guard = CAMERA_DISCOVERY_PHASE_TRANSITION_LOCK
        .lock()
        .map_err(|_| "camera discovery phase transition lock poisoned".to_string())?;
    crate::camera_probe::stop_discovery()?;
    let previous_phase_id = active
        .camera_discovery
        .as_ref()
        .map_or("unknown", |phase| phase.phase_id.as_str());
    log_and_reset_camera_hook_entry_counters(previous_phase_id);
    let selection = DEPTH_SELECTION
        .lock()
        .map_err(|_| "depth selection lock poisoned during discovery phase reset".to_string())?;
    let current = CURRENT_SELECTED_DEPTH
        .lock()
        .map_err(|_| "selected depth lock poisoned during discovery phase reset".to_string())?;
    let current_pair = (!DSV_BINDING_DIRTY.load(Ordering::Acquire))
        .then(|| current.as_ref().map(SelectedDepth::pair))
        .flatten();
    let recovery = phase_reset_depth_recovery(&selection, current_pair, &expected)?;
    if matches!(
        (&expected, recovery),
        (
            DepthSelectionExpectation::ExactDescriptor(_),
            PhaseDepthRecovery::ReacquireOnDraw { .. }
        )
    ) {
        let retained_pair = match RETAINED_UNBOUND_SELECTED_DEPTH.lock() {
            Ok(retained) => retained.as_ref().map(SelectedDepth::pair),
            Err(poisoned) => {
                drop(poisoned.into_inner());
                return Err("retained unbound DSV owner lock poisoned during phase reset".into());
            }
        };
        if !exact_recovery_retained_owner_matches(&expected, recovery, retained_pair) {
            return Err(format!(
                "phase reset lost the strong owner for its unbound exact DSV pair: recovery={recovery:?} retained={retained_pair:?}"
            ));
        }
    }
    log(&format!(
        "legacy camera discovery phase={} reset DSV state: selected={} current={} strategy={}",
        discovery.phase_id,
        format_resource(recovery.selected_resource()),
        format_resource(recovery.current_resource()),
        recovery.strategy_name(),
    ));
    configure_discovery_dsv_recovery(recovery);
    CAMERA_HOOK_INTEGRITY_MARKED.store(false, Ordering::Release);
    CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.store(false, Ordering::Release);
    VIEWPORT_OBSERVATION_ERROR_LOGGED.store(false, Ordering::Release);
    CAMERA_HOOK_AUDIT_DUE.store(true, Ordering::Release);
    let result = crate::camera_probe::init_discovery(
        std::path::PathBuf::from(&next.output_dir),
        discovery,
        replayed_descriptor,
        ACTIVE_GENERATION.load(Ordering::Acquire),
    );
    if result.is_err() {
        DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
        DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
    }
    result
}

fn camera_discovery_phase_reset_config(
    active: &depth_lab::probe_config::ProbeConfig,
    next: &depth_lab::probe_config::ProbeConfig,
) -> Result<depth_lab::CameraDiscoveryConfig, String> {
    let mut active_control = active.clone();
    let mut next_control = next.clone();
    active_control.camera_discovery = None;
    next_control.camera_discovery = None;
    if active_control != next_control || !next.camera_probe {
        return Err(
            "session/output/depth control identity changed outside camera_discovery".into(),
        );
    }
    let previous = active
        .camera_discovery
        .as_ref()
        .ok_or("active camera discovery config missing")?;
    let discovery = next
        .camera_discovery
        .as_ref()
        .ok_or("next camera discovery config missing")?;
    if previous.schema != discovery.schema {
        return Err(format!(
            "camera discovery schema changed: active={} next={}",
            previous.schema, discovery.schema
        ));
    }
    if previous.executable_sha256 != discovery.executable_sha256 {
        return Err("camera discovery executable identity changed".into());
    }
    if previous.render != discovery.render {
        return Err("camera discovery render identity changed".into());
    }
    if previous.dsv_selection != discovery.dsv_selection {
        return Err("camera discovery DSV attestation locator/hash binding changed".into());
    }
    Ok(discovery.clone())
}

fn phase_reset_depth_recovery(
    selection: &DepthSelectionState,
    current_pair: Option<DsvPairIdentity>,
    expected: &DepthSelectionExpectation,
) -> Result<PhaseDepthRecovery, String> {
    if selection.expected.as_ref() != Some(expected) {
        return Err(
            "active depth selection identity does not match the replayed next phase".into(),
        );
    }
    if selection.ambiguous {
        return Err("active depth selection is ambiguous and cannot cross a phase reset".into());
    }
    match (selection.selected_pair, current_pair) {
        (None, None) => Ok(PhaseDepthRecovery::AwaitInitialSelection),
        (Some(pair), None) => Ok(PhaseDepthRecovery::ReacquireOnDraw { pair }),
        (Some(selected), Some(current)) if selection.identity(selected).matches_pair(current) => {
            Ok(PhaseDepthRecovery::PreserveCurrent { pair: current })
        }
        (Some(selected), Some(current)) => Err(format!(
            "current DSV pair does not match the uniquely selected pair: selected={selected:?} current={current:?}"
        )),
        (None, Some(current)) => Err(format!(
            "current DSV {current:?} has no uniquely selected pair identity"
        )),
    }
}

fn exact_recovery_retained_owner_matches(
    expected: &DepthSelectionExpectation,
    recovery: PhaseDepthRecovery,
    retained_pair: Option<DsvPairIdentity>,
) -> bool {
    match (expected, recovery) {
        (
            DepthSelectionExpectation::ExactDescriptor(_),
            PhaseDepthRecovery::ReacquireOnDraw { pair },
        ) => retained_pair == Some(pair),
        _ => true,
    }
}

fn configure_discovery_dsv_recovery(recovery: PhaseDepthRecovery) {
    let pair = match recovery {
        PhaseDepthRecovery::ReacquireOnDraw { pair } => Some(pair),
        PhaseDepthRecovery::AwaitInitialSelection | PhaseDepthRecovery::PreserveCurrent { .. } => {
            None
        }
    };
    DISCOVERY_DSV_REACQUIRE_LAST_PRESENT.store(u64::MAX, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_ERROR_LOGGED.store(false, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_SUCCESS_LOGGED.store(false, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_RESOURCE.store(
        pair.map_or(0, |pair| pair.resource_tag as usize),
        Ordering::Release,
    );
    DISCOVERY_DSV_REACQUIRE_VIEW.store(
        pair.map_or(0, |pair| pair.view_tag as usize),
        Ordering::Release,
    );
}

fn format_resource(resource: Option<u64>) -> String {
    resource.map_or_else(|| "none".into(), |resource| format!("{resource:#x}"))
}

pub(crate) struct PeerProcess {
    handle: HANDLE,
    process_id: u32,
}

// SAFETY: owns a process handle used only by non-blocking waits and CloseHandle.
// Moving the owner transfers the one close obligation, never the foreign process state.
unsafe impl Send for PeerProcess {}

impl PeerProcess {
    pub(crate) fn open(process_id: u32) -> Result<Self, String> {
        let handle =
            unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, process_id) }.map_err(|error| {
                format!("OpenProcess({process_id}) for peer liveness failed: {error}")
            })?;
        Ok(Self { handle, process_id })
    }

    pub(crate) fn has_exited(&self) -> Result<bool, String> {
        classify_peer_wait(unsafe { WaitForSingleObject(self.handle, 0) }).map_err(|error| {
            format!(
                "client process {} liveness check failed: {error}",
                self.process_id
            )
        })
    }
}

impl Drop for PeerProcess {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

fn classify_peer_wait(wait: WAIT_EVENT) -> Result<bool, String> {
    if wait == WAIT_OBJECT_0 {
        Ok(true)
    } else if wait == WAIT_TIMEOUT {
        Ok(false)
    } else if wait == WAIT_FAILED {
        Err(windows::core::Error::from_thread().to_string())
    } else {
        Err(format!("unexpected wait result {}", wait.0))
    }
}

#[derive(Clone, Copy, Debug)]
enum PeerLifecyclePhase {
    Starting,
    Recording,
    Sealed,
    Stopping,
}

fn peer_liveness_fault(
    process_id: u32,
    phase: PeerLifecyclePhase,
    result: Result<bool, String>,
) -> Option<String> {
    match result {
        Ok(false) => None,
        Ok(true) => Some(format!(
            "client process {process_id} exited during {phase:?}"
        )),
        Err(error) => Some(format!(
            "client process {process_id} liveness failed during {phase:?}: {error}"
        )),
    }
}

struct ControlRun {
    start_command_id: String,
    identity: depth_lab::StreamIdentity,
    output_dir: std::path::PathBuf,
    actual_profile: depth_lab::DepthProfileIdentity,
    actual_binding: depth_lab::CameraBindingIdentity,
    depth_profile_sha256: String,
    camera_binding_sha256: String,
    ordinal: u32,
    ready_sent: bool,
    warmup_started_at: Instant,
    stopping: Option<StoppingRun>,
    /// Once a valid StopRun has been observed, every terminal ACK belongs to
    /// that command even if the run faults before Stopped can be emitted.
    pending_stop_command_id: Option<String>,
    selected_present_runtime: Option<SelectedPresentProbeRuntime>,
    selected_present_expected: bool,
    selected_present_resource_epoch: u64,
    peer_process: Option<PeerProcess>,
    d3d12_hook_timing_baseline: Option<BTreeMap<String, crate::hook_timing::TimingSnapshot>>,
}

struct StoppingRun {
    command_id: String,
    cutoff_qpc: i64,
    cutoff_present_seq: u64,
    started_at: Instant,
    d3d12_hook_timing_end: Option<BTreeMap<String, crate::hook_timing::TimingSnapshot>>,
}

/// 指针链后台审计周期。各游戏按审计成本设置独立的 verdict TTL，
/// 控制线程偶发调度延迟不会造成无辜的 pose 诊断行。
const POINTER_AUDIT_PERIOD: Duration = Duration::from_millis(100);

fn control_loop() {
    let control_path = depth_lab::probe_control::control_path();
    let mut last_command_id = String::new();
    let mut last_generation = 0u64;
    let mut active: Option<ControlRun> = None;
    let mut last_pointer_audit: Option<Instant> = None;

    loop {
        if let Ok(command) = depth_lab::probe_control::load_command_from(&control_path)
            && command.command_id != last_command_id
        {
            last_command_id = command.command_id.clone();
            match &command.command {
                depth_lab::ProbeControlCommand::StartRun(start) => {
                    if start.target_process_id != std::process::id() {
                        log(&format!(
                            "discarded StartRun for process {} in process {}",
                            start.target_process_id,
                            std::process::id()
                        ));
                    } else if command.identity.generation <= last_generation {
                        log(&format!(
                            "discarded stale StartRun generation={} last={last_generation}",
                            command.identity.generation
                        ));
                    } else if active.is_some() {
                        write_fault(
                            &std::path::PathBuf::from(&start.output_dir),
                            &command,
                            1,
                            "start",
                            "run_already_active",
                            "a previous depth generation is still active",
                            None,
                        );
                    } else {
                        let mut startup_timing = crate::startup_timing::StartupTiming::begin(
                            command.identity.generation,
                        );
                        match crate::startup_timing::phase("prepare_control_run", || {
                            prepare_control_run(&command, start)
                        }) {
                            Ok(mut run) => {
                                let start_result = start_depth_run(
                                    &control_probe_config(&command, start),
                                    command.identity.generation,
                                    true,
                                    start.pass_identity_trace,
                                    start.performance_metrics_enabled,
                                    start.performance_validation_only,
                                )
                                .and_then(|()| {
                                    configure_local_compatibility(
                                        &command.identity,
                                        start.required_present_fps,
                                    )?;
                                    install_selected_present_runtime(
                                        run.selected_present_runtime.take(),
                                    )
                                });
                                if let Err(error) = start_result {
                                    if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() {
                                        bootstrap.backend().set_runtime_observation_active(false);
                                    }
                                    crate::perf::finish_run();
                                    let _ = clear_selected_present_runtime();
                                    let _ = stop_depth_writer(command.identity.generation);
                                    let _ = stop_active_projection(command.identity.generation);
                                    write_fault(
                                        &run.output_dir,
                                        &command,
                                        1,
                                        "start",
                                        "writer_or_camera_start_failed",
                                        &error,
                                        None,
                                    );
                                    begin_stop_camera_production();
                                    let _ = finish_stop_camera_production();
                                    if let Ok(mut config) = ACTIVE_CONFIG.lock() {
                                        *config = None;
                                    }
                                } else {
                                    // Warmup timeout starts only after every writer/configuration step has
                                    // succeeded. Startup I/O must not consume the render-stability budget.
                                    run.warmup_started_at = Instant::now();
                                    ACTIVE_GENERATION
                                        .store(command.identity.generation, Ordering::Release);
                                    DEPTH_CAPTURE_ENABLED.store(true, Ordering::Release);
                                    run.ordinal = 1;
                                    write_control_ack(
                                        &run.output_dir,
                                        depth_lab::ProbeAck {
                                            schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                                            command_id: run.start_command_id.clone(),
                                            identity: run.identity.clone(),
                                            ordinal: run.ordinal,
                                            ack: depth_lab::ProbeAckKind::Started,
                                        },
                                    );
                                    startup_timing.mark_started();
                                    active = Some(run);
                                }
                            }
                            Err(error) => write_fault(
                                &std::path::PathBuf::from(&start.output_dir),
                                &command,
                                1,
                                "start",
                                "contract_invalid",
                                &error,
                                None,
                            ),
                        }
                    }
                }
                depth_lab::ProbeControlCommand::StopRun(stop) => {
                    let Some(run) = active.as_mut() else {
                        log("discarded StopRun because no generation is active");
                        std::thread::sleep(Duration::from_millis(50));
                        continue;
                    };
                    if run.identity != command.identity {
                        log(&format!(
                            "discarded StopRun identity mismatch: command={:?} active={:?}",
                            command.identity, run.identity
                        ));
                    } else if run.stopping.is_some() {
                        log("discarded duplicate StopRun while flush is in progress");
                    } else {
                        run.pending_stop_command_id = Some(command.command_id.clone());
                        if run.selected_present_expected
                            && !SELECTED_PRESENT_SEALED.load(Ordering::Acquire)
                        {
                            latch_selected_present_fault(
                                depth_lab::selected_present_feedback::FaultReason::StopBeforeGenerationSealed,
                                "StopRun arrived before GenerationSealed",
                            );
                        } else {
                            let _present_guard = PRESENT_RUN_LOCK
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            FORMAL_CAPTURE_ARMED.store(false, Ordering::Release);
                            DEPTH_CAPTURE_ENABLED.store(false, Ordering::Release);
                            begin_stop_camera_production();
                            DRAINING.store(true, Ordering::Release);
                            // The last Present can leave a source-copy request open.
                            // Close it under the capture mutex before observing idle;
                            // ResourceBarrier/fallback callbacks do not hold the Present lock.
                            if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get()
                                && let Err(error) =
                                    bootstrap.backend().stop_depth_capture_production()
                            {
                                drop(_present_guard);
                                let run = active.take().expect("StopRun has an active run");
                                last_generation = run.identity.generation;
                                abort_control_run(run, "flush", "capture_stop_failed", &error);
                                continue;
                            }
                            let cutoff_present_seq = camera_last_submitted_present_seq()
                                .unwrap_or_else(|| {
                                    PRESENT_SEQ.load(Ordering::Acquire).saturating_sub(1)
                                });
                            // The cutoff and production gates are frozen. Slow ACK/report
                            // writes must not keep D3D12 rendering behind the lifecycle lock.
                            if D3D12_EARLY_BOOTSTRAP.get().is_some() {
                                drop(_present_guard);
                            }
                            // Freeze relaxed hook counters before ACK/verdict file IO. Descriptor
                            // hooks intentionally remain live while the game process does, so a
                            // later snapshot would charge flush-time rendering to the run window.
                            let d3d12_hook_timing_end = run
                                .d3d12_hook_timing_baseline
                                .as_ref()
                                .and_then(|_| D3D12_EARLY_BOOTSTRAP.get())
                                .map(|bootstrap| bootstrap.backend().hook_timing_snapshot());
                            run.ordinal += 1;
                            write_control_ack(
                                &run.output_dir,
                                depth_lab::ProbeAck {
                                    schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                                    command_id: command.command_id.clone(),
                                    identity: run.identity.clone(),
                                    ordinal: run.ordinal,
                                    ack: depth_lab::ProbeAckKind::Stopped {
                                        cutoff_qpc: stop.cutoff_qpc,
                                        cutoff_present_seq,
                                    },
                                },
                            );
                            write_present_rate_verdict(&run.output_dir);
                            run.stopping = Some(StoppingRun {
                                command_id: command.command_id.clone(),
                                cutoff_qpc: stop.cutoff_qpc,
                                cutoff_present_seq,
                                started_at: Instant::now(),
                                d3d12_hook_timing_end,
                            });
                        }
                    }
                }
            }
        }

        let peer_fault = active
            .as_ref()
            .and_then(|run| {
                run.peer_process.as_ref().map(|peer| {
                    let phase = if run.stopping.is_some() {
                        PeerLifecyclePhase::Stopping
                    } else if !run.ready_sent {
                        PeerLifecyclePhase::Starting
                    } else if SELECTED_PRESENT_SEALED.load(Ordering::Acquire) {
                        PeerLifecyclePhase::Sealed
                    } else {
                        PeerLifecyclePhase::Recording
                    };
                    peer_liveness_fault(peer.process_id, phase, peer.has_exited())
                })
            })
            .flatten();
        if let Some(message) = peer_fault
            && let Some(run) = active.take()
        {
            last_generation = run.identity.generation;
            publish_selected_present_fault(
                run.identity.generation,
                depth_lab::selected_present_feedback::FaultReason::PeerExited,
            );
            abort_control_run(
                run,
                "lifecycle",
                depth_lab::selected_present_feedback::FaultReason::PeerExited.code(),
                &message,
            );
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let compatibility_error = active
            .as_ref()
            .filter(|run| run.stopping.is_none())
            .and_then(|run| local_compatibility_error(&run.identity));
        if let Some(message) = compatibility_error
            && let Some(run) = active.take()
        {
            last_generation = run.identity.generation;
            abort_control_run(
                run,
                "local_compatibility",
                "present_rate_mismatch",
                &message,
            );
            continue;
        }

        let camera_failure = active.as_ref().and_then(|_| camera_runtime_failure());
        if let Some(message) = camera_failure
            && let Some(run) = active.take()
        {
            last_generation = run.identity.generation;
            let reason = SELECTED_PRESENT_FAULT
                .lock()
                .ok()
                .and_then(|fault| fault.as_ref().map(|(reason, _)| *reason))
                .unwrap_or(depth_lab::selected_present_feedback::FaultReason::IdentityMismatch);
            publish_selected_present_fault(run.identity.generation, reason);
            abort_control_run(
                run,
                "camera",
                camera_production_fault_code(&message),
                &format!("Camera production failed: {message}"),
            );
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let selected_present_fault = SELECTED_PRESENT_FAULT
            .lock()
            .ok()
            .and_then(|mut fault| fault.take());
        if let Some((reason, message)) = selected_present_fault
            && let Some(run) = active.take()
        {
            last_generation = run.identity.generation;
            publish_selected_present_fault(run.identity.generation, reason);
            abort_control_run(
                run,
                "feedback",
                reason.code(),
                &format!("selected-Present feedback failed: {message}"),
            );
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        // 连续异常自动停录（130 号 §6.5）。writer 线程只 latch，拆运行在这里。
        //
        // **先留现场再拆**：`abort_control_run` 会停 writer 并关掉 camera production，
        // 那之后 `raw/` 还在，但 probe 日志会继续被别的 run 追加，窗口的索引行也可能
        // 被下一次采集覆盖。现场必须在这一刻定格。
        let health_abort = HEALTH_ABORT.lock().ok().and_then(|mut abort| abort.take());
        if let Some(mut abort) = health_abort {
            let Some(run) = active.take() else {
                // 没有活动 run 时不该有中止信号；丢掉它并留痕，
                // 但**不要**留在锁里等下一个 run——那会让新 run 一起步就被停。
                log(&format!(
                    "discarded depth frame health abort without an active run: {}",
                    abort.diagnostic()
                ));
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            abort.scene_errors = depth_lab::health_abort::preserve_scene(
                &run.output_dir,
                probe_log_path().as_deref(),
                &abort,
            );
            let diagnostic = abort.diagnostic();
            if let Err(error) = depth_lab::health_abort::write_record(&run.output_dir, &abort) {
                log(&format!("留存自动停录记录失败: {error}"));
            }
            log(&format!(
                "depth frame health abort — 停录并留存现场\n{diagnostic}"
            ));
            last_generation = run.identity.generation;
            abort_control_run(
                run,
                "frame_health",
                depth_lab::health_abort::HEALTH_ABORT_FAULT_CODE,
                &diagnostic,
            );
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        if let Some(run) = active.as_mut()
            && !run.ready_sent
            && run.stopping.is_none()
        {
            let first_present_seq = FIRST_PRESENT_SEQ.load(Ordering::Acquire);
            let actual_swapchain_adapter = ACTUAL_SWAPCHAIN_ADAPTER
                .lock()
                .ok()
                .and_then(|adapter| adapter.clone());
            if first_present_seq != u64::MAX
                && let Some(actual_swapchain_adapter) = actual_swapchain_adapter
            {
                if let Err(error) = persist_local_compatibility(&run.identity, &run.output_dir) {
                    let run = active.take().expect("active checked");
                    last_generation = run.identity.generation;
                    abort_control_run(run, "local_compatibility", "evidence_missing", &error);
                    continue;
                }
                let timing_window = if crate::perf::enabled() {
                    let mut qpc_frequency = 0i64;
                    unsafe { QueryPerformanceFrequency(&mut qpc_frequency) }
                        .map_err(|error| format!("QueryPerformanceFrequency: {error}"))
                        .and_then(|()| {
                            crate::perf::begin_certification_window(
                                current_qpc(),
                                qpc_frequency,
                                PERF_CERTIFICATION_SETTLE_SECONDS,
                                PERF_CERTIFICATION_MEASUREMENT_SECONDS,
                            )
                        })
                } else {
                    Ok(())
                };
                if let Err(error) = timing_window {
                    let run = active.take().expect("active run checked above");
                    last_generation = run.identity.generation;
                    abort_control_run(run, "performance", "present_timing_start_failed", &error);
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
                if crate::perf::enabled() {
                    run.d3d12_hook_timing_baseline = D3D12_EARLY_BOOTSTRAP
                        .get()
                        .map(|bootstrap| bootstrap.backend().hook_timing_snapshot());
                }
                run.ordinal += 1;
                let ready_result = depth_lab::probe_control::write_ack(
                    &run.output_dir,
                    &depth_lab::ProbeAck {
                        schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                        command_id: run.start_command_id.clone(),
                        identity: run.identity.clone(),
                        ordinal: run.ordinal,
                        ack: depth_lab::ProbeAckKind::Ready {
                            actual_profile: run.actual_profile.clone(),
                            actual_binding: run.actual_binding.clone(),
                            depth_profile_sha256: run.depth_profile_sha256.clone(),
                            camera_binding_sha256: run.camera_binding_sha256.clone(),
                            // Independent probe releases have their own runtime identity. Local
                            // developer builds fall back to the workspace package version, while
                            // depth-probe-release.yml always supplies the reviewed policy value.
                            probe_version: compiled_probe_runtime_version().into(),
                            actual_swapchain_adapter,
                            first_present_seq,
                            resource_epoch: run.selected_present_resource_epoch,
                            performance_metrics_enabled: crate::perf::enabled(),
                        },
                    },
                );
                if let Err(error) = ready_result {
                    let run = active.take().expect("active run checked above");
                    last_generation = run.identity.generation;
                    abort_control_run(run, "control", "ready_ack_write_failed", &error.to_string());
                    continue;
                }
                run.ready_sent = true;
            }
        }

        let warmup_fault = active.as_ref().and_then(|run| {
            if run.ready_sent || run.stopping.is_some() || formal_capture_armed() {
                return None;
            }
            let explicit = CAPTURE_WARMUP_ERROR
                .lock()
                .ok()
                .and_then(|mut error| error.take());
            explicit.map_or_else(
                || {
                    let timeout = depth_lab::probe_control::capture_warmup_timeout(
                        &run.actual_binding.schema,
                    );
                    capture_warmup_timed_out(run.warmup_started_at, Instant::now(), timeout).then(|| {
                        let diagnostics = CAPTURE_WARMUP
                            .lock()
                            .ok()
                            .and_then(|warmup| {
                                warmup.as_ref().map(CaptureWarmup::diagnostic_summary)
                            })
                            .unwrap_or_else(|| "warmup diagnostics unavailable".into());
                        (
                            "warmup_timeout",
                            format!(
                                "capture warmup did not become stable within {} seconds; {diagnostics}",
                                timeout.as_secs()
                            ),
                        )
                    })
                },
                |error| Some(("warmup_failed", error)),
            )
        });
        if let Some((code, message)) = warmup_fault {
            let run = active.take().expect("warmup fault requires an active run");
            last_generation = run.identity.generation;
            abort_control_run(run, "warmup", code, &message);
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }

        let d3d12_stopping = active.as_ref().is_some_and(|run| run.stopping.is_some())
            && D3D12_EARLY_BOOTSTRAP.get().is_some_and(|bootstrap| {
                bootstrap.backend().capture_slots_in_flight() != 0
                    || d3d12_selected_in_flight() != 0
            });
        if d3d12_stopping
            && active
                .as_ref()
                .and_then(|run| run.stopping.as_ref())
                .is_some_and(|stop| stop.started_at.elapsed() > Duration::from_secs(5))
        {
            let run = active.take().expect("D3D12 stopping run exists");
            last_generation = run.identity.generation;
            abort_control_run(
                run,
                "flush",
                "readback_timeout",
                "D3D12 GPU work did not drain within five seconds",
            );
            continue;
        }
        if d3d12_stopping {
            let swapchain =
                D3D12_ACTIVE_SWAPCHAIN.load(Ordering::Acquire) as *mut core::ffi::c_void;
            let generation = active.as_ref().map_or(0, |run| run.identity.generation);
            let drained = if swapchain.is_null() {
                Err("D3D12 stopping swapchain is missing".to_string())
            } else {
                queue_d3d12_depth_frames(
                    swapchain,
                    PRESENT_SEQ.load(Ordering::Acquire),
                    generation,
                    true,
                )
            };
            if let Err(error) = drained {
                let run = active.take().expect("D3D12 stopping run exists");
                last_generation = run.identity.generation;
                abort_control_run(run, "flush", "readback_failed", &error);
                continue;
            }
        }
        let d3d12_in_flight = D3D12_EARLY_BOOTSTRAP
            .get()
            .is_some_and(|bootstrap| bootstrap.backend().capture_slots_in_flight() != 0);
        let can_finish = active.as_ref().is_some_and(|run| {
            run.stopping.is_some()
                && crate::readback::in_flight() == 0
                && !d3d12_in_flight
                && d3d12_selected_in_flight() == 0
        });
        if can_finish {
            // Match Present's lock order: lifecycle -> depth send. Zero GPU slots
            // does not mean a CPU Present callback has finished using the runtime.
            let present_guard =
                lock_d3d12_teardown(&PRESENT_RUN_LOCK, D3D12_EARLY_BOOTSTRAP.get().is_some());
            let send_lock = DEPTH_SEND_LOCK.lock();
            if send_lock.is_ok()
                && (crate::readback::in_flight() != 0
                    || D3D12_EARLY_BOOTSTRAP.get().is_some_and(|bootstrap| {
                        bootstrap.backend().capture_slots_in_flight() != 0
                    })
                    || d3d12_selected_in_flight() != 0)
            {
                // A render callback that had already entered before StopRun
                // issued one final GPU copy. Keep draining; do not close the
                // writer on the stale zero observed above.
                drop(send_lock);
                drop(present_guard);
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            let mut run = active.take().expect("checked above");
            let stopping = run.stopping.take().expect("checked above");
            let mut selected_present_summary = None;
            let mut swapchain_cleanup_error = None;
            let depth_summary = match send_lock {
                Ok(_guard) => {
                    ACTIVE_GENERATION.store(0, Ordering::Release);
                    selected_present_summary = clear_selected_present_runtime();
                    if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get()
                        && bootstrap.backend().capture_is_armed()
                        && let Err(error) = bootstrap.backend().disarm_depth_capture()
                    {
                        swapchain_cleanup_error = Some(error);
                    }
                    if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() {
                        bootstrap.backend().set_runtime_observation_active(false);
                    }
                    D3D12_ACTIVE_SWAPCHAIN.store(0, Ordering::Release);
                    if let Err(error) = clear_observed_swapchain_devices() {
                        swapchain_cleanup_error = Some(error);
                    }
                    // Admission now bypasses retired D3D12 state. Let the game
                    // keep presenting while file writers perform their bounded flush.
                    drop(present_guard);
                    stop_depth_writer(run.identity.generation)
                }
                Err(_) => {
                    drop(present_guard);
                    Err("depth send lock poisoned during flush".into())
                }
            };
            let camera_summary = finish_stop_camera_production();
            let mut camera_errors = camera_summary.errors;
            if !d3d12_presentation_only()
                && (run.ready_sent || camera_summary.last_present_seq.is_some())
                && camera_summary.last_present_seq != Some(stopping.cutoff_present_seq)
            {
                camera_errors.push(format!(
                    "Camera writer last present {:?} does not match cutoff {}",
                    camera_summary.last_present_seq, stopping.cutoff_present_seq
                ));
            }
            let readback_completed = selected_present_summary
                .as_ref()
                .and_then(|summary| summary.d3d12_readback_completed)
                .unwrap_or_else(|| readback::stats().map_or(0, |stats| stats.completed));
            let gpu_allocation_error = selected_present_summary
                .as_ref()
                .and_then(|selected| selected.gpu_allocation_error.clone());
            let selected_present =
                selected_present_summary.map(|selected| depth_lab::SelectedPresentFlushSummary {
                    generation: selected.generation,
                    resource_epoch: selected.resource_epoch,
                    first_candidate_present_seq: selected.first_candidate_present_seq,
                    materialized_candidates: selected.materialized_candidates,
                    selected_frame_count: selected.selected_frame_count,
                    distinct_selected_depth_count: selected.distinct_selected_depth_count,
                    source_identity_alias_count: selected.source_identity_alias_count,
                    readback_submitted_count: selected.readback_submitted_count,
                    readback_completed_count: readback_completed,
                    unresolved_candidates: selected.unresolved_candidates,
                    ring_overwrite_count: 0,
                    max_unresolved_candidates: selected.max_unresolved_candidates,
                    max_feedback_lag_presents: selected.max_feedback_lag_presents,
                    feedback_latency: selected.feedback_latency,
                    gpu_allocation: selected.gpu_allocation.map(|stats| {
                        depth_lab::CandidateGpuAllocationSummary {
                            capacity: stats.capacity,
                            texture_bytes: stats.texture_bytes,
                            estimated_bytes: stats.estimated_bytes,
                            configured_limit_bytes: stats.configured_limit_bytes,
                            configured_reserve_bytes: stats.configured_reserve_bytes,
                            budget_before: stats.budget_before,
                            usage_before: stats.usage_before,
                            headroom_before: stats.headroom_before,
                            budget_after: stats.budget_after,
                            usage_after: stats.usage_after,
                            headroom_after: stats.headroom_after,
                            actual_usage_delta: stats.actual_usage_delta,
                            budget_at_flush: stats.budget_at_flush,
                            usage_at_flush: stats.usage_at_flush,
                            headroom_at_flush: stats.headroom_at_flush,
                        }
                    }),
                    feedback_queue_depth: selected.feedback_queue_depth,
                    ack_queue_depth: selected.ack_queue_depth,
                    feedback_queue_max_depth: selected.feedback_queue_max_depth,
                    ack_queue_max_depth: selected.ack_queue_max_depth,
                    feedback_queue_full_count: selected.feedback_queue_full_count,
                    ack_queue_full_count: selected.ack_queue_full_count,
                    transport_fault_reason: selected.transport_fault_reason,
                });
            let mut qpf = 0i64;
            if unsafe { QueryPerformanceFrequency(&mut qpf) }.is_err() {
                qpf = 0;
            }
            let present_timing = match crate::perf::present_timing_summary(stopping.cutoff_qpc, qpf)
            {
                Ok(timing) => timing,
                Err(error) => {
                    camera_errors.push(format!("Present timing summary failed: {error}"));
                    None
                }
            };
            let mut summary = depth_lab::ProbeFlushSummary {
                writer_queue_depth: WRITER_QUEUE_DEPTH.load(Ordering::Acquire),
                last_present_seq: stopping.cutoff_present_seq,
                cutoff_present_seq: stopping.cutoff_present_seq,
                depth_row_count: 0,
                depth_file_count: 0,
                camera_pose_row_count: camera_summary.pose_row_count,
                camera_diagnostic_row_count: camera_summary.diagnostic_row_count,
                player_pose_row_count: flushed_player_pose_row_count(
                    d3d12_presentation_only(),
                    camera_summary.player_pose_row_count,
                ),
                selected_present,
                writer_bypass: writer_bypass(),
                writer_bypassed_frames: WRITER_BYPASSED.load(Ordering::Acquire),
                writer_timing: crate::perf::encode_timing_summary(),
                performance_metrics_enabled: crate::perf::enabled(),
                present_timing,
                errors: camera_errors,
            };
            if let Some(error) = swapchain_cleanup_error {
                summary.errors.push(error);
            }
            if run.selected_present_expected && summary.selected_present.is_none() {
                summary
                    .errors
                    .push("selected-Present runtime summary is missing".into());
            }
            if let Err(error) = stop_active_projection(run.identity.generation) {
                summary.errors.push(error);
            }
            match depth_summary {
                Ok(depth) => {
                    summary.depth_row_count = depth.row_count;
                    summary.depth_file_count = depth.file_count;
                    if depth
                        .last_depth_present_seq
                        .is_some_and(|last| last > stopping.cutoff_present_seq)
                    {
                        summary.errors.push(format!(
                            "depth writer accepted a late frame after cutoff {}",
                            stopping.cutoff_present_seq
                        ));
                    }
                    summary.errors.extend(depth.errors);
                }
                Err(error) => summary.errors.push(error),
            }
            let dropped = WRITER_QUEUE_DROPPED.load(Ordering::Acquire);
            if dropped != 0 {
                summary
                    .errors
                    .push(format!("depth writer queue dropped {dropped} frames"));
            }
            run.ordinal += 1;
            if let (Some(start), Some(end)) = (
                run.d3d12_hook_timing_baseline.as_ref(),
                stopping.d3d12_hook_timing_end.as_ref(),
            ) && let Err(error) =
                write_d3d12_hot_path_metrics(&run.output_dir, run.identity.generation, start, end)
            {
                log(&format!("D3D12 热路径遥测落盘失败: {error}"));
                summary
                    .errors
                    .push(format!("D3D12 hot-path metrics dump failed: {error}"));
            }
            // 逐帧遥测随 Flush 落盘（默认关闭时是 no-op）。放在 Flushed ack 之前：
            // 客户端看到 Flushed 时 perf-frames.jsonl 已经完整。
            if let Err(error) = crate::perf::dump_rings(&run.output_dir, qpf) {
                log(&format!("perf 逐帧遥测落盘失败: {error}"));
                summary
                    .errors
                    .push(format!("perf metrics dump failed: {error}"));
            }
            match summary.validate() {
                Ok(()) => {
                    write_control_ack(
                        &run.output_dir,
                        depth_lab::ProbeAck {
                            schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
                            command_id: stopping.command_id,
                            identity: run.identity.clone(),
                            ordinal: run.ordinal,
                            ack: depth_lab::ProbeAckKind::Flushed { summary },
                        },
                    );
                }
                Err(error) => {
                    let validation_diagnostics = if summary.errors.is_empty() {
                        String::new()
                    } else {
                        format!("; diagnostics: {}", summary.errors.join("; "))
                    };
                    let message = gpu_allocation_error.as_ref().map_or_else(
                        || {
                            format!(
                                "flush summary validation failed: {error}{validation_diagnostics}"
                            )
                        },
                        |allocation_error| {
                            format!(
                                "GPU allocation finalization failed: {allocation_error}; flush summary validation failed: {error}{validation_diagnostics}"
                            )
                        },
                    );
                    write_fault(
                        &run.output_dir,
                        &depth_lab::ProbeControlEnvelope {
                            schema: depth_lab::probe_control::CONTROL_SCHEMA_V3.into(),
                            command_id: stopping.command_id,
                            identity: run.identity.clone(),
                            command: depth_lab::ProbeControlCommand::StopRun(depth_lab::StopRun {
                                cutoff_qpc: stopping.cutoff_qpc,
                                reason: "flush".into(),
                            }),
                        },
                        run.ordinal,
                        "flush",
                        "flush_validation_failed",
                        &message,
                        Some(summary),
                    );
                }
            }
            last_generation = run.identity.generation;
            DRAINING.store(false, Ordering::Release);
            PRESENT_ONLY.store(false, Ordering::Release);
            WATERMARK_ONLY.store(false, Ordering::Release);
            WRITER_BYPASS.store(false, Ordering::Release);
            FORMAL_CAPTURE_ARMED.store(false, Ordering::Release);
            if let Ok(mut warmup) = CAPTURE_WARMUP.lock() {
                *warmup = None;
            }
            if let Ok(mut error) = CAPTURE_WARMUP_ERROR.lock() {
                *error = None;
            }
            if let Ok(mut config) = ACTIVE_CONFIG.lock() {
                *config = None;
            }
            let _ = crate::readback::reset_if_drained();
            crate::perf::finish_run();
        }

        // Camera/Player 指针链周期审计：production 期唯一的 VirtualQuery 执行点。
        // 与 StartRun/StopRun 同线程串行——StartRun 置位 enabled 后本轮循环尾即首审
        // （warmup readiness 依赖首个结论），Stop 清空结论后不存在迟到写入。
        if depth_capture_enabled()
            && last_pointer_audit
                .is_none_or(|last_audit| last_audit.elapsed() >= POINTER_AUDIT_PERIOD)
        {
            last_pointer_audit = Some(Instant::now());
            crate::camera_probe::run_background_pointer_audit(
                ACTIVE_GENERATION.load(Ordering::Acquire),
                current_qpc(),
            );
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}

fn preserve_d3d12_warmup_failure(run: &ControlRun, message: &mut String) {
    if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() {
        // Capture before teardown for every pre-Ready abort, including camera
        // faults handled earlier than the timeout branch in the control loop.
        // This runs once on the control thread, never on Present/Draw.
        let observation = bootstrap.backend().snapshot();
        let arm_error = D3D12_LAST_ARM_ERROR
            .lock()
            .ok()
            .and_then(|error| error.clone());
        message.push_str(&format!(
            "; d3d12_arm_error={arm_error:?}; d3d12_blockers={:?}; capture_error={:?}",
            observation.blockers, observation.capture_error
        ));
        let report = serde_json::json!({
            "schema": "dm-d3d12-warmup-failure/v1",
            "identity": run.identity,
            "arm_error": arm_error,
            "observation": observation,
            "selected_capture_safe": bootstrap.backend().selected_capture_safe(),
            "capture_lifecycle": {
                "present_seq": PRESENT_SEQ.load(Ordering::Acquire),
                "formal_armed": formal_capture_armed(),
                "first_formal_present": FIRST_PRESENT_SEQ.load(Ordering::Acquire),
                "selected_enabled": SELECTED_PRESENT_ENABLED.load(Ordering::Acquire),
                "selected_armed": SELECTED_PRESENT_ARMED.load(Ordering::Acquire),
                "selected_sealed": SELECTED_PRESENT_SEALED.load(Ordering::Acquire),
                "issued_depth_frames": CAPTURE_ISSUED.load(Ordering::Acquire),
            },
            "render_camera": bootstrap.backend().render_camera_snapshot(),
            "kcd2_warmup": crate::kcd2_live_pose::warmup_diagnostics(),
        });
        let result = serde_json::to_vec_pretty(&report)
            .map_err(|error| error.to_string())
            .and_then(|bytes| {
                std::fs::write(run.output_dir.join("d3d12-warmup-failure.json"), bytes)
                    .map_err(|error| error.to_string())
            });
        if let Err(error) = result {
            message.push_str(&format!("; warmup_report_write_failed={error}"));
        }
    }
}

fn abort_control_run(mut run: ControlRun, phase: &str, code: &str, message: &str) {
    let mut diagnostic_message = message.to_owned();
    if !run.ready_sent {
        preserve_d3d12_warmup_failure(&run, &mut diagnostic_message);
    }
    let message = diagnostic_message.as_str();
    let mut cleanup_errors = Vec::new();
    let fault_command_id = active_fault_command_id(
        &run.start_command_id,
        run.pending_stop_command_id.as_deref(),
    )
    .to_string();
    {
        let _present_guard = PRESENT_RUN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        FORMAL_CAPTURE_ARMED.store(false, Ordering::Release);
        DEPTH_CAPTURE_ENABLED.store(false, Ordering::Release);
        ACTIVE_GENERATION.store(0, Ordering::Release);
        if let Err(error) = clear_observed_swapchain_devices() {
            cleanup_errors.push(error);
        }
        match crate::readback::abort_faulted_generation() {
            Ok(aborted) if !aborted.is_empty() => log(&format!(
                "faulted generation {} explicitly abandoned selected readbacks {:?}",
                run.identity.generation, aborted
            )),
            Ok(_) => {}
            Err(error) => cleanup_errors.push(error),
        }
        match CAPTURE_PASS_EVIDENCE.lock() {
            Ok(mut evidence) => evidence.clear(),
            Err(_) => cleanup_errors.push("capture pass evidence lock poisoned".into()),
        }
        let _ = clear_selected_present_runtime();
        if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get()
            && let Err(error) = bootstrap.backend().abort_depth_capture()
        {
            cleanup_errors.push(error);
        }
        if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() {
            bootstrap.backend().set_runtime_observation_active(false);
        }
        begin_stop_camera_production();
    }
    if let Err(error) = stop_depth_writer(run.identity.generation) {
        cleanup_errors.push(error);
    }
    if let Err(error) = stop_active_projection(run.identity.generation) {
        cleanup_errors.push(error);
    }
    cleanup_errors.extend(finish_stop_camera_production().errors);
    cleanup_errors.retain(|error| !message.contains(error.as_str()));
    if !crate::readback::reset_if_drained() {
        cleanup_errors.push("faulted readback state did not reset".into());
    }
    let full_message = if cleanup_errors.is_empty() {
        message.to_string()
    } else {
        format!("{message}; cleanup errors: {}", cleanup_errors.join("; "))
    };
    run.ordinal += 1;
    write_fault_for_command(
        &run.output_dir,
        &fault_command_id,
        &run.identity,
        run.ordinal,
        phase,
        code,
        &full_message,
        None,
    );
    DRAINING.store(false, Ordering::Release);
    PRESENT_ONLY.store(false, Ordering::Release);
    WATERMARK_ONLY.store(false, Ordering::Release);
    WRITER_BYPASS.store(false, Ordering::Release);
    if let Ok(mut warmup) = CAPTURE_WARMUP.lock() {
        *warmup = None;
    }
    if let Ok(mut config) = ACTIVE_CONFIG.lock() {
        *config = None;
    }
    crate::perf::finish_run();
}

fn camera_production_fault_code(message: &str) -> &'static str {
    if message.contains("player pose sample") || message.contains("actor-root transform") {
        "player_pose_sampling_failed"
    } else if message.contains("camera pose sample") {
        "camera_pose_sampling_failed"
    } else {
        "camera_production_failed"
    }
}

fn active_fault_command_id<'a>(
    start_command_id: &'a str,
    stop_command_id: Option<&'a str>,
) -> &'a str {
    stop_command_id.unwrap_or(start_command_id)
}

fn read_verified_snapshot(
    path: &std::path::Path,
    expected_sha256: &str,
    label: &str,
) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};

    let bytes = std::fs::read(path)
        .map_err(|error| format!("read {label} {} failed: {error}", path.display()))?;
    let actual = format!("sha256:{:x}", Sha256::digest(&bytes));
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(format!(
            "{label} hash mismatch: expected={expected_sha256} actual={actual} path={}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn prepare_control_run(
    command: &depth_lab::ProbeControlEnvelope,
    start: &depth_lab::StartRun,
) -> Result<ControlRun, String> {
    let actual_process_id = std::process::id();
    if start.target_process_id != actual_process_id {
        return Err(format!(
            "StartRun target process mismatch: requested={} actual={actual_process_id}",
            start.target_process_id
        ));
    }
    let profile_bytes = read_verified_snapshot(
        std::path::Path::new(&start.depth_profile_path),
        &start.depth_profile_sha256,
        "depth profile",
    )?;
    let profile: depth_lab::DepthProfile = serde_json::from_slice(&profile_bytes)
        .map_err(|error| format!("parse depth profile failed: {error}"))?;
    let binding_bytes = read_verified_snapshot(
        std::path::Path::new(&start.camera_binding_path),
        &start.camera_binding_sha256,
        "CameraBinding",
    )?;
    let binding: depth_lab::CameraBindingDocument = serde_json::from_slice(&binding_bytes)
        .map_err(|error| format!("parse CameraBinding failed: {error}"))?;
    binding.validate_for_profile(&profile, &current_executable_sha256()?)?;
    let peer_process = start
        .selected_present_feedback
        .as_ref()
        .map(|bootstrap| {
            bootstrap.validate().map_err(|error| error.to_string())?;
            PeerProcess::open(bootstrap.client_process_id)
        })
        .transpose()?;
    let selected_present_runtime = start
        .selected_present_feedback
        .as_ref()
        .map(|bootstrap| {
            let stream = depth_lab::selected_present_feedback::StreamEpoch {
                identity: depth_lab::selected_present_feedback::WireIdentity::new(
                    &command.identity.session_id,
                    &command.identity.run_id,
                )
                .map_err(|error| error.to_string())?,
                generation: command.identity.generation,
                resource_epoch: bootstrap.resource_epoch,
            };
            let names = depth_lab::selected_present_transport::TransportNames::new(
                bootstrap.client_process_id,
                actual_process_id,
                command.identity.generation,
                &bootstrap.nonce,
            )
            .map_err(|error| error.to_string())?;
            let transport =
                depth_lab::selected_present_transport::ProbeTransport::open(&names, &stream)
                    .map_err(|error| error.to_string())?;
            let ring_state = depth_lab::selected_present_candidate_ring::CandidateRingState::new(
                depth_lab::selected_present_candidate_ring::CandidateRingConfig {
                    capacity: bootstrap.ring_capacity,
                    max_feedback_lag_presents: bootstrap.max_feedback_lag_presents,
                    guard_slots: bootstrap.guard_slots,
                },
                stream.generation,
                stream.resource_epoch,
            )
            .map_err(|error| error.to_string())?;
            let mut qpc_frequency = 0i64;
            unsafe { QueryPerformanceFrequency(&mut qpc_frequency) }
                .map_err(|error| format!("QueryPerformanceFrequency: {error}"))?;
            let feedback_latency = FeedbackLatencySamples::new(
                u64::try_from(qpc_frequency)
                    .map_err(|_| "QPC frequency is not positive".to_string())?,
            )?;
            Ok::<_, String>(SelectedPresentProbeRuntime {
                transport,
                state: depth_lab::selected_present_feedback::FeedbackState::new(stream.clone()),
                ring_state,
                gpu_ring: None,
                d3d12_gpu_ring: None,
                candidate_evidence: BTreeMap::new(),
                candidate_conv: None,
                readback_slots: profile.readback_slots,
                bootstrap: bootstrap.clone(),
                stream,
                pending_arm: None,
                next_ack_ordinal: 1,
                feedback_latency,
            })
        })
        .transpose()?;
    let selected_present_expected = selected_present_runtime.is_some();
    let selected_present_resource_epoch = start
        .selected_present_feedback
        .as_ref()
        .map_or(1, |bootstrap| bootstrap.resource_epoch);
    Ok(ControlRun {
        start_command_id: command.command_id.clone(),
        identity: command.identity.clone(),
        output_dir: std::path::PathBuf::from(&start.output_dir),
        actual_profile: depth_lab::DepthProfileIdentity {
            schema_version: profile.schema_version.clone(),
            game_id: profile.game_id.clone(),
            lane: match profile.lane {
                depth_lab::Lane::Lane30 => "lane30",
                depth_lab::Lane::Lane60 => "lane60",
            }
            .into(),
            executable_sha256: profile.executable_sha256,
        },
        actual_binding: binding.identity(),
        depth_profile_sha256: start.depth_profile_sha256.clone(),
        camera_binding_sha256: start.camera_binding_sha256.clone(),
        ordinal: 0,
        ready_sent: false,
        warmup_started_at: Instant::now(),
        stopping: None,
        pending_stop_command_id: None,
        selected_present_runtime,
        selected_present_expected,
        selected_present_resource_epoch,
        peer_process,
        d3d12_hook_timing_baseline: None,
    })
}

fn control_probe_config(
    command: &depth_lab::ProbeControlEnvelope,
    start: &depth_lab::StartRun,
) -> depth_lab::probe_config::ProbeConfig {
    depth_lab::probe_config::ProbeConfig {
        enabled: true,
        output_dir: start.output_dir.clone(),
        unbind_from_end: start.unbind_from_end,
        max_frames: start.max_frames,
        present_only: start.present_only,
        watermark_only: start.watermark_only,
        writer_bypass: start.writer_bypass,
        target_depth_hz: Some(start.target_depth_hz),
        lane_fps: start.lane_fps,
        camera_probe: false,
        camera_discovery: None,
        representative_scene_attestation_sha256: None,
        dsv_discovery: None,
        camera_production: Some(depth_lab::probe_config::CameraProductionConfig {
            binding_path: start.camera_binding_path.clone(),
            binding_sha256: Some(start.camera_binding_sha256.clone()),
            run_id: command.identity.run_id.clone(),
            generation: command.identity.generation,
        }),
        depth_profile_path: Some(start.depth_profile_path.clone()),
        depth_profile_sha256: Some(start.depth_profile_sha256.clone()),
        session_id: command.identity.session_id.clone(),
    }
}

fn write_control_ack(output_dir: &std::path::Path, ack: depth_lab::ProbeAck) {
    if let Err(error) = depth_lab::probe_control::write_ack(output_dir, &ack) {
        log(&format!("write probe lifecycle ACK failed: {error}"));
    }
}

/// StopRun 时把 present 帧率裁决落进采集输出目录。deliver 读到 violated 即
/// 拒绝出件；线下重建也绕不开这个文件——这就是「拒绝出证」的证。
/// 闸未启用（旧客户端没发 lane_fps）则不写文件，deliver 侧视为无裁决可依。
fn write_present_rate_verdict(output_dir: &std::path::Path) {
    // 取裁决和写文件分开：锁只护取数那一瞬。若停止瞬间还有一个 Present 越过
    // 采集开关的窗口期在等这把锁，不能让它陪着磁盘 I/O 一起等。
    let verdict = {
        let Ok(guard) = PRESENT_RATE_GATE.lock() else {
            return;
        };
        let Some(gate) = guard.as_ref() else {
            return;
        };
        gate.verdict()
    };
    let path = output_dir.join(depth_lab::present_rate::VERDICT_FILE_NAME);
    match serde_json::to_vec_pretty(&verdict) {
        Ok(bytes) => {
            if let Err(error) = std::fs::write(&path, bytes) {
                log(&format!("写 present-rate-verdict 失败: {error}"));
            } else if verdict.violated {
                log(&format!(
                    "present-rate-verdict: violated=true（观测 {:.2} fps vs 车道 {}），本条不可交付",
                    verdict.observed_fps_at_worst, verdict.lane_fps
                ));
            }
        }
        Err(error) => log(&format!("序列化 present-rate-verdict 失败: {error}")),
    }
}

#[derive(Serialize)]
struct D3d12HotPathDelta {
    calls: u64,
    total_us: u64,
}

#[derive(Serialize)]
struct D3d12HotPathArtifact<'a> {
    schema: &'static str,
    generation: u64,
    scope: &'static str,
    measurement: &'static str,
    start: &'a BTreeMap<String, crate::hook_timing::TimingSnapshot>,
    end: &'a BTreeMap<String, crate::hook_timing::TimingSnapshot>,
    delta: BTreeMap<String, D3d12HotPathDelta>,
}

fn write_d3d12_hot_path_metrics(
    output_dir: &std::path::Path,
    generation: u64,
    start: &BTreeMap<String, crate::hook_timing::TimingSnapshot>,
    end: &BTreeMap<String, crate::hook_timing::TimingSnapshot>,
) -> Result<(), String> {
    let names = start
        .keys()
        .chain(end.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let delta = names
        .into_iter()
        .map(|name| {
            let start = start.get(&name).cloned().unwrap_or_default();
            let end = end.get(&name).cloned().unwrap_or_default();
            (
                name,
                D3d12HotPathDelta {
                    calls: end.calls.saturating_sub(start.calls),
                    total_us: end.total_us.saturating_sub(start.total_us),
                },
            )
        })
        .collect();
    let artifact = D3d12HotPathArtifact {
        schema: "dm-perf-d3d12-hot-path-v1",
        generation,
        scope: "ready_to_stop; relaxed counter checkpoints",
        measurement: "wall_elapsed_including_lock_wait; concurrent_or_nested_stages_are_not_additive; endpoint_max_us_is_lifetime_only",
        start,
        end,
        delta,
    };
    let path = output_dir.join("perf-d3d12-hot-path.json");
    let bytes = serde_json::to_vec_pretty(&artifact)
        .map_err(|error| format!("serialize {} failed: {error}", path.display()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create {} failed: {error}", path.display()))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write {} failed: {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("sync {} failed: {error}", path.display()))
}

#[allow(clippy::too_many_arguments)]
fn write_fault(
    output_dir: &std::path::Path,
    command: &depth_lab::ProbeControlEnvelope,
    ordinal: u32,
    phase: &str,
    code: &str,
    message: &str,
    summary: Option<depth_lab::ProbeFlushSummary>,
) {
    write_fault_for_command(
        output_dir,
        &command.command_id,
        &command.identity,
        ordinal,
        phase,
        code,
        message,
        summary,
    );
}

#[allow(clippy::too_many_arguments)]
fn write_fault_for_command(
    output_dir: &std::path::Path,
    command_id: &str,
    identity: &depth_lab::StreamIdentity,
    ordinal: u32,
    phase: &str,
    code: &str,
    message: &str,
    summary: Option<depth_lab::ProbeFlushSummary>,
) {
    write_control_ack(
        output_dir,
        depth_lab::ProbeAck {
            schema: depth_lab::probe_control::ACK_SCHEMA_V3.into(),
            command_id: command_id.to_string(),
            identity: identity.clone(),
            ordinal,
            ack: depth_lab::ProbeAckKind::Faulted {
                phase: phase.into(),
                code: code.into(),
                message: message.into(),
                summary,
            },
        },
    );
}

/// 读取本次运行的配置：配置文件优先，回落到环境变量（手工流程）。
fn resolved_config() -> Option<depth_lab::probe_config::ProbeConfig> {
    if let Ok(active) = ACTIVE_CONFIG.lock()
        && let Some(config) = active.as_ref()
    {
        return Some(config.clone());
    }
    if let Ok(cfg) = depth_lab::probe_config::load() {
        return Some(cfg);
    }
    let output_dir = std::env::var("DM_DEPTH_PROBE_OUTPUT_DIR").ok()?;
    Some(depth_lab::probe_config::ProbeConfig {
        enabled: true,
        output_dir,
        unbind_from_end: std::env::var("DM_DEPTH_PROBE_UNBIND_FROM_END")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4),
        max_frames: std::env::var("DM_DEPTH_PROBE_MAX_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok()),
        present_only: std::env::var_os("DM_DEPTH_PROBE_PRESENT_ONLY").is_some(),
        // 手工流程也能测仅水印档：从同一个 shell 设 DM_DEPTH_PROBE_WATERMARK_ONLY
        watermark_only: std::env::var_os("DM_DEPTH_PROBE_WATERMARK_ONLY").is_some(),
        // 手工 A/B 仅用于工程排障；控制协议仍会强制 validation-only。
        writer_bypass: std::env::var_os("DM_DEPTH_PROBE_WRITER_BYPASS").is_some(),
        target_depth_hz: std::env::var("DM_DEPTH_PROBE_TARGET_HZ")
            .ok()
            .and_then(|v| v.parse().ok()),
        // 手工流程不带车道号：帧率闸停用（会留一行日志）。手工产物本就不可交付。
        lane_fps: None,
        camera_probe: std::env::var_os("DM_DEPTH_PROBE_CAMERA").is_some(),
        camera_discovery: None,
        representative_scene_attestation_sha256: None,
        dsv_discovery: None,
        camera_production: None,
        depth_profile_path: std::env::var("DM_DEPTH_PROBE_PROFILE").ok(),
        depth_profile_sha256: None,
        session_id: "manual".into(),
    })
}

/// 从逐游戏 profile 解析连续层节拍。旧配置、候选投影和手工流程都保持定频默认值。
fn resolved_depth_cadence() -> depth_lab::DepthCadence {
    let Some(config) = resolved_config() else {
        return depth_lab::DepthCadence::TargetHz;
    };
    let Some(path) = config.depth_profile_path.as_deref() else {
        return depth_lab::DepthCadence::TargetHz;
    };
    let bytes = match config.depth_profile_sha256.as_deref() {
        Some(expected) => {
            read_verified_snapshot(std::path::Path::new(path), expected, "depth profile")
        }
        None => std::fs::read(path).map_err(|error| error.to_string()),
    };
    let text =
        match bytes.and_then(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string())) {
            Ok(text) => text,
            Err(error) => {
                log(&format!(
                    "读取 depth profile cadence 失败（{path}: {error}），保持 target_hz"
                ));
                return depth_lab::DepthCadence::TargetHz;
            }
        };
    match serde_json::from_str::<depth_lab::DepthProfile>(&text) {
        Ok(profile) => profile.cadence,
        Err(error) => {
            log(&format!(
                "解析 depth profile cadence 失败（{path}: {error}），保持 target_hz"
            ));
            depth_lab::DepthCadence::TargetHz
        }
    }
}

fn init_depth_worker() -> Result<(), String> {
    let (tx, rx) = sync_channel::<DepthWriterCommand>(DEPTH_WRITER_QUEUE_CAPACITY);
    std::thread::Builder::new()
        .name("dm-depth-writer".into())
        .spawn(move || depth_writer_loop(rx))
        .map_err(|error| format!("start depth writer thread failed: {error}"))?;
    DEPTH_TX
        .set(tx)
        .map_err(|_| "depth writer worker already initialized".to_string())
}

fn open_pass_identity_trace(
    root: &std::path::Path,
    enabled: bool,
) -> Result<Option<BufWriter<std::fs::File>>, String> {
    if !enabled {
        return Ok(None);
    }
    let path = root.join(PASS_IDENTITY_TRACE_FILE);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|error| format!("create {} failed: {error}", path.display()))?;
    Ok(Some(BufWriter::new(file)))
}

fn validate_depth_writer_dsv_id(
    d3d12_dsv_id: Option<&str>,
    pass: &depth_lab::capture_timing::DsvPassEvidence,
) -> Result<(), String> {
    let Some(expected) = d3d12_dsv_id else {
        // D3D11's selection ids name frame-local resource observations, not the
        // reviewed profile DSV. Preserve that path's historical index identity.
        return Ok(());
    };
    pass.validate()?;
    let selection = &pass.selection;
    if expected.trim().is_empty()
        || selection.selected_id != expected
        || !selection.operator_exact_pin
        || selection.any_msaa
    {
        return Err(format!(
            "D3D12 depth writer selected DSV does not match the run profile: expected={expected} selected={} exact_pin={} any_msaa={}",
            selection.selected_id, selection.operator_exact_pin, selection.any_msaa
        ));
    }
    Ok(())
}

fn depth_writer_loop(rx: std::sync::mpsc::Receiver<DepthWriterCommand>) {
    struct Run {
        generation: u64,
        writer: DepthWriter,
        d3d12_dsv_id: Option<String>,
        /// 本次 run 的落盘变换（94 号）。
        ///
        /// profile 给了 Z 约定时开工就定死；**profile 缺失时是 `None`**，
        /// 由前几帧的清除值反推，定了就不再改——中途换变换会让同一次采集里
        /// 两段数据的还原方式不同，虽然索引逐行记得住，但排障时没人会想到去看。
        transform: Option<depth_lab::capture_timing::StorageTransform>,
        /// 已经就「profile 与数据不一致」报过警了。只报一次，
        /// 否则 15 fps 会把日志刷爆，真正的信息反而看不见。
        warned_convention_mismatch: bool,
        /// Runtime-only escalating alerts. Durable per-frame evidence lives in depth-index/v5;
        /// final deliverability is decided later with frame pairs and gameplay state.
        health_streak: depth_lab::frame_health::DefectStreak,
        /// 中止时要留存的现场窗口。只滚动帧号，不留像素——见
        /// [`depth_lab::health_abort::SceneWindow`]。
        health_scene: depth_lab::health_abort::SceneWindow,
        identity_trace: Option<BufWriter<std::fs::File>>,
        identity_trace_rows: u64,
        summary: DepthWriterSummary,
    }

    /// 这一帧要落哪种变换。
    ///
    /// 两条路，分工刻意不同：
    ///
    /// - **profile 给了约定**：以它为准，但仍拿数据反推来核对。不一致时
    ///   **不偷偷改**，只报一次警——config 是人填的真值来源，代码擅自推翻它
    ///   会让同一次采集里两段数据的还原方式不同，而人还以为 profile 生效了。
    ///   报警之后由人去修 profile 重采。
    /// - **profile 没给**：只能靠反推。第一个判得出来的答案定死，之后不再改。
    ///
    /// # 空帧是可判的，而且是最可靠的一种
    ///
    /// 判据是清除值占了一大片，而空帧整帧都是清除值——清成 1.0 只可能是传统 Z，
    /// 清成 0.0 只可能是 reversed-Z。所以开局的加载画面通常一帧就把变换定下来了，
    /// **这正是想要的**：等到第一帧有几何再定，前面那些帧已经按错的变换落完了。
    ///
    /// 真正判不出来的是「两端都没有大片清除值」的帧（纯几何铺满屏），那时按 identity
    /// 落并且**不冻结**，下一帧接着判。
    fn decide_transform(
        run: &mut Run,
        depth: &[f32],
        present_seq: u64,
    ) -> depth_lab::capture_timing::StorageTransform {
        use depth_lab::capture_timing::{StorageTransform, infer_z_convention};

        if let Some(fixed) = run.transform {
            if !run.warned_convention_mismatch
                && let Some(seen) = infer_z_convention(depth)
                && StorageTransform::for_convention(seen) != fixed
            {
                run.warned_convention_mismatch = true;
                log(&format!(
                    "profile 的 Z 约定与数据不一致（seq {present_seq}：数据看起来是 {seen:?}，\
                     profile 定的落盘变换是 {fixed:?}）。**本次采集仍按 profile 落盘**，\
                     不擅自改；请核对 depth profile 的 projection_b 符号后重采（94 号）"
                ));
            }
            return fixed;
        }

        let Some(seen) = infer_z_convention(depth) else {
            return StorageTransform::Identity;
        };
        let decided = StorageTransform::for_convention(seen);
        run.transform = Some(decided);
        log(&format!(
            "Z 约定由数据反推为 {seen:?}（seq {present_seq}），\
             本次采集落盘变换定为 {decided:?}，之后不再改"
        ));
        decided
    }

    set_current_thread_lossless_writer_priority();
    let mut run: Option<Run> = None;
    let mut decoded_depth = Vec::new();
    while let Ok(command) = rx.recv() {
        match command {
            DepthWriterCommand::Start {
                generation,
                root,
                projection,
                d3d12_dsv_id,
                pass_identity_trace,
                reply,
            } => {
                let result = if run.is_some() {
                    Err("previous depth writer generation is still active".into())
                } else {
                    match DepthWriter::new(&root) {
                        Ok(writer) => {
                            let identity_trace =
                                match open_pass_identity_trace(&root, pass_identity_trace) {
                                    Ok(writer) => writer,
                                    Err(error) => {
                                        let _ = reply.send(Err(error));
                                        continue;
                                    }
                                };
                            let transform = transform_from_profile(&projection);
                            match transform {
                                Some(t) => log(&format!("落盘变换按 profile 定为 {t:?}")),
                                None => log("profile 没给 Z 约定：落盘变换将由清除值反推。\
                                     开局的加载画面整帧都是清除值，通常一帧就定得下来；\
                                     定下来之前按 identity 落盘"),
                            }
                            run = Some(Run {
                                generation,
                                writer,
                                d3d12_dsv_id,
                                transform,
                                warned_convention_mismatch: false,
                                health_streak: depth_lab::frame_health::DefectStreak::default(),
                                health_scene: depth_lab::health_abort::SceneWindow::new(),
                                identity_trace,
                                identity_trace_rows: 0,
                                summary: DepthWriterSummary::default(),
                            });
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                };
                let _ = reply.send(result);
            }
            DepthWriterCommand::Frame(pending) => {
                WRITER_QUEUE_DEPTH.fetch_sub(1, Ordering::AcqRel);
                let Some(active) = run.as_mut() else {
                    log("discarded late depth frame after writer stop");
                    continue;
                };
                if pending.generation != active.generation {
                    log(&format!(
                        "discarded stale depth frame generation={} active={}",
                        pending.generation, active.generation
                    ));
                    continue;
                }
                if !active.summary.errors.is_empty() {
                    continue;
                }
                if let Err(error) =
                    validate_depth_writer_dsv_id(active.d3d12_dsv_id.as_deref(), &pending.pass)
                {
                    active.summary.errors.push(format!(
                        "depth writer source identity failed at {}: {error}",
                        pending.frame.present_seq
                    ));
                    continue;
                }
                let frame = pending.frame;
                let source_format = frame.format;
                let present_seq = frame.present_seq;
                let perf_prepare =
                    crate::perf::enabled().then(|| (current_qpc(), std::time::Instant::now()));
                if let Err(error) = frame.decode_into(&mut decoded_depth) {
                    active
                        .summary
                        .errors
                        .push(format!("depth decode failed at {present_seq}: {error}"));
                    continue;
                }
                let depth = &decoded_depth;
                // GPU_POOL_VERIFY 影子对拍：CPU 参考（上面的 depth）与 GPU 池化必须逐位一致。
                // 任何 diff 都进 Flush errors——诊断模式下宁可判废整个 run，也不放过分叉。
                if let Some(shadow) = frame.verify_pooled.as_deref() {
                    match crate::readback::decode_pooled_bytes(
                        shadow,
                        frame.dst_width,
                        frame.dst_height,
                        frame.verify_row_pitch,
                    ) {
                        Ok(gpu_depth) => {
                            if let Some(mismatch) =
                                crate::readback::verify_bitwise(depth, &gpu_depth, frame.dst_width)
                            {
                                active
                                    .summary
                                    .errors
                                    .push(format!("GPU_POOL_VERIFY 帧 {present_seq}: {mismatch}"));
                            }
                        }
                        Err(error) => {
                            active.summary.errors.push(format!(
                                "GPU_POOL_VERIFY 帧 {present_seq} 影子解码失败: {error}"
                            ));
                        }
                    }
                }
                let transform = decide_transform(active, depth, present_seq);
                let preparation = perf_prepare.map(|(start, timer)| {
                    (
                        start,
                        u64::try_from(timer.elapsed().as_nanos()).unwrap_or(u64::MAX),
                    )
                });
                // writer 线程的 EXR 编码+写盘计时进独立 encode 环：慢帧若与 34–111 ms 的
                // encode 时间窗重叠（内存带宽竞争嫌疑），离线按 QPC 区间即可定位。
                let perf_encode =
                    crate::perf::enabled().then(|| (current_qpc(), std::time::Instant::now()));
                let write_result = active.writer.write_frame(
                    present_seq,
                    frame.dst_width as u32,
                    frame.dst_height as u32,
                    source_format,
                    active.d3d12_dsv_id.as_deref().unwrap_or(MAIN_DSV_ID),
                    pending.pass,
                    depth,
                    pending.readback_latency_frames,
                    pending.projection,
                    transform,
                );
                if let Some((start_qpc, started)) = perf_encode {
                    crate::perf::record_encode(
                        present_seq,
                        start_qpc,
                        u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
                        preparation,
                    );
                }
                match write_result {
                    Ok(health) => {
                        active.summary.row_count += 1;
                        active.summary.file_count += 1;
                        active.summary.last_depth_present_seq = Some(present_seq);
                        if let Some(writer) = active.identity_trace.as_mut() {
                            let trace_result = pending.identity_trace.as_ref().ok_or_else(|| {
                                format!(
                                    "pass identity trace is enabled but frame {present_seq} has no trace"
                                )
                            }).and_then(|trace| {
                                serde_json::to_writer(&mut *writer, trace)
                                    .map_err(|error| format!("serialize pass identity trace: {error}"))?;
                                writer
                                    .write_all(b"\n")
                                    .map_err(|error| format!("write pass identity trace: {error}"))
                            });
                            match trace_result {
                                Ok(()) => active.identity_trace_rows += 1,
                                Err(error) => active.summary.errors.push(error),
                            }
                        }
                        let defective = health.is_defective();
                        // 现场窗口先于判据推进：到线那一帧本身也是异常段的一员，
                        // 而窗口的中心是这一段的**第一帧**，两者用的是同一个 `defective`。
                        active.health_scene.record(defective, present_seq);
                        let verdict = active.health_streak.record(defective, present_seq);
                        if !matches!(verdict, depth_lab::frame_health::StreakVerdict::Continue) {
                            let (defective_frames, total) = active.health_streak.totals();
                            let (level, streak) = match verdict {
                                depth_lab::frame_health::StreakVerdict::Alert(n) => ("alert", n),
                                depth_lab::frame_health::StreakVerdict::Abort(n) => ("ABORT", n),
                                depth_lab::frame_health::StreakVerdict::Continue => unreachable!(),
                            };
                            log(&format!(
                                "depth frame health {level} seq={present_seq} streak={streak} \
                                 defects={:?} raw_unique={} stored_unique={} \
                                 defective={defective_frames}/{total}; stop and inspect before a long capture",
                                health.defects,
                                health.raw.unique_sampled,
                                health.stored.unique_sampled,
                            ));
                            // 连续 300 帧异常 = 系统性问题（DSV 选错 / 快照环失效 /
                            // 场景切换未适配）。#518 那次烧掉了 4 分钟才被发现。
                            //
                            // **只留证据、不自行停也不自行重启**：停录要拆的东西归控制
                            // 线程串行拥有（见 `HEALTH_ABORT`），重启则只会再废一次并冲掉现场。
                            //
                            // `summary.errors` 那条照旧要写：控制线程若因为任何原因没能
                            // 走到中止（例如 run 正好在同一刻被 StopRun 拿走），
                            // Flush 侧仍然必须拒绝这份数据，而不是静默放行。
                            if matches!(verdict, depth_lab::frame_health::StreakVerdict::Abort(_))
                                && !HEALTH_ABORT_LATCHED.swap(true, Ordering::AcqRel)
                            {
                                active.summary.errors.push(format!(
                                    "depth frame health ABORT at present_seq {present_seq}: \
                                     {streak} consecutive defective frames"
                                ));
                                let abort = depth_lab::health_abort::HealthAbort::new(
                                    active
                                        .health_streak
                                        .first_defective_seq()
                                        .unwrap_or(present_seq),
                                    present_seq,
                                    streak,
                                    health.defects.clone(),
                                    defective_frames,
                                    total,
                                    active.health_scene.present_seqs(),
                                );
                                match HEALTH_ABORT.lock() {
                                    Ok(mut latched) => *latched = Some(abort),
                                    // 锁中毒还去 unwrap 就是把「停录」变成「崩在游戏进程里」。
                                    // 记一条，Flush 侧的 errors 已经足以让这份数据出不去。
                                    Err(_) => {
                                        log("depth frame health ABORT 无法 latch：中止锁已中毒，\
                                         本次只能靠 Flush errors 拒绝数据")
                                    }
                                }
                            }
                        }
                    }
                    Err(error) => active
                        .summary
                        .errors
                        .push(format!("depth writer failed at {present_seq}: {error}")),
                }
            }
            DepthWriterCommand::Stop { generation, reply } => {
                let result = match run.take() {
                    Some(mut active) if active.generation == generation => {
                        decoded_depth = Vec::new();
                        if let Some(writer) = active.identity_trace.as_mut()
                            && let Err(error) = writer.flush()
                        {
                            active
                                .summary
                                .errors
                                .push(format!("flush pass identity trace failed: {error}"));
                        }
                        if active.identity_trace.is_some() {
                            log(&format!(
                                "pass identity trace summary generation={generation} rows={}",
                                active.identity_trace_rows
                            ));
                        }
                        let (defective, total) = active.health_streak.totals();
                        log(&format!(
                            "depth frame health summary generation={generation} \
                             defective={defective}/{total} longest_streak={}",
                            active.health_streak.longest()
                        ));
                        Ok(active.summary)
                    }
                    Some(active) => {
                        let actual = active.generation;
                        run = Some(active);
                        Err(format!(
                            "depth writer stop generation mismatch: expected {actual}, got {generation}"
                        ))
                    }
                    None => Err("depth writer is not active".into()),
                };
                let _ = reply.send(result);
            }
        }
    }
}

fn start_depth_run(
    config: &depth_lab::probe_config::ProbeConfig,
    generation: u64,
    defer_formal_capture: bool,
    pass_identity_trace: bool,
    performance_metrics_enabled: bool,
    performance_validation_only: bool,
) -> Result<(), String> {
    config.validate().map_err(|error| error.to_string())?;
    crate::kcd2_live_pose::reset_for_next_run()?;
    EXACT_PRODUCTION_SNAPSHOT_ENABLED.store(false, Ordering::Release);
    // Managed runs configure A3 timing explicitly through StartRun. No
    // process, Steam, or machine environment is consulted here.
    crate::perf::begin_run(performance_metrics_enabled);
    if defer_formal_capture && config.camera_production.is_none() {
        return Err("lifecycle warmup requires Camera production evidence".into());
    }
    FORMAL_CAPTURE_ARMED.store(false, Ordering::Release);
    *CAPTURE_WARMUP_ERROR
        .lock()
        .map_err(|_| "capture warmup error lock poisoned".to_string())? = None;
    let resolved_projection = resolve_projection_for_run(config);
    if !crate::readback::reset_if_drained() {
        return Err("previous GPU readback generation is not drained".into());
    }
    *CURRENT_SELECTED_DEPTH
        .lock()
        .map_err(|_| "selected depth lock poisoned during generation reset".to_string())? = None;
    *RETAINED_UNBOUND_SELECTED_DEPTH.lock().map_err(|_| {
        "retained unbound depth lock poisoned during generation reset".to_string()
    })? = None;
    CURRENT_SELECTED_RAW.store(0, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_RESOURCE.store(0, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_VIEW.store(0, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_LAST_PRESENT.store(u64::MAX, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_ERROR_LOGGED.store(false, Ordering::Release);
    DISCOVERY_DSV_REACQUIRE_SUCCESS_LOGGED.store(false, Ordering::Release);
    DSV_BINDING_DIRTY.store(true, Ordering::Release);
    DSV_REACQUIRE_LAST_PRESENT.store(u64::MAX, Ordering::Release);
    CAMERA_HOOK_INTEGRITY_MARKED.store(false, Ordering::Release);
    CAMERA_HOOK_INTEGRITY_ERROR_LOGGED.store(false, Ordering::Release);
    VIEWPORT_OBSERVATION_ERROR_LOGGED.store(false, Ordering::Release);
    CAMERA_HOOK_AUDIT_DUE.store(true, Ordering::Release);
    BACKING_VTABLE_PRESENT_TRANSITION_LOGGED.store(false, Ordering::Release);
    POST_PRESENT_SHADOW_REPAIR_LOGGED.store(false, Ordering::Release);
    reset_camera_hook_entry_counters();
    *COMPLETED_DEPTH_SNAPSHOTS
        .lock()
        .map_err(|_| "depth snapshot lock poisoned during generation reset".to_string())? = None;
    CAPTURE_PASS_EVIDENCE
        .lock()
        .map_err(|_| "DSV pass evidence lock poisoned during generation reset".to_string())?
        .clear();
    OBSERVED_DRAW_VIEWPORTS
        .lock()
        .map_err(|_| "exact-draw viewport lock poisoned during generation reset".to_string())?
        .clear();
    PASS_IDENTITY_TRACE_ENABLED.store(pass_identity_trace, Ordering::Release);
    *SELECTED_DSV_UNBINDS.lock().map_err(|_| {
        "selected DSV unbind counter lock poisoned during generation reset".to_string()
    })? = SelectedDsvUnbindCounter {
        present_seq: u64::MAX,
        count: 0,
    };
    dsv_frame_tracker()
        .lock()
        .map_err(|_| "DSV frame tracker lock poisoned during generation reset".to_string())?
        .reset();
    DSV_RANK_MISMATCH_LOGGED.store(false, Ordering::Release);
    DSV_RANK_AMBIGUOUS_LOGGED.store(false, Ordering::Release);
    DSV_RANK_MSAA_LOGGED.store(false, Ordering::Release);
    SELECTED_PASS_DRAW_CALLS.store(0, Ordering::Release);
    let selected_dsv_descriptor =
        crate::startup_timing::phase("depth_selection", || configure_depth_selection(config))?;
    let root = std::path::PathBuf::from(&config.output_dir);
    if config.camera_probe {
        audit_camera_hook_integrity_before_phase_reset()?;
        CAMERA_HOOK_INTEGRITY_MARKED.store(false, Ordering::Release);
        let discovery = config
            .camera_discovery
            .clone()
            .ok_or("camera_probe 缺 camera_discovery")?;
        let actual_executable_sha256 = current_executable_sha256()?;
        if discovery.executable_sha256 != actual_executable_sha256 {
            return Err(format!(
                "camera discovery executable hash 漂移: config={} actual={actual_executable_sha256}",
                discovery.executable_sha256
            ));
        }
        crate::camera_probe::init_discovery(
            root.clone(),
            discovery,
            selected_dsv_descriptor,
            generation,
        )?;
    }
    if config.camera_production.is_some() {
        crate::startup_timing::phase("camera_production", || {
            init_camera_production(config, root.clone(), performance_validation_only)
        })?;
        if defer_formal_capture {
            begin_camera_production_warmup(generation)?;
        }
    }
    *ACTIVE_CONFIG
        .lock()
        .map_err(|_| "active config lock poisoned".to_string())? = Some(config.clone());
    let projection = resolved_projection.map_or_else(
        depth_lab::capture_timing::IndexProjection::unknown,
        |value| value.index,
    );
    let d3d12_dsv_id = D3D12_DEPTH_PROFILE
        .lock()
        .map_err(|_| "D3D12 depth profile lock poisoned at writer start")?
        .as_ref()
        .map(|(selector, _)| selector.candidate_id.clone());
    let (reply_tx, reply_rx) = sync_channel(1);
    DEPTH_TX
        .get()
        .ok_or("depth writer worker is not initialized")?
        .send(DepthWriterCommand::Start {
            generation,
            root,
            projection,
            d3d12_dsv_id,
            pass_identity_trace,
            reply: reply_tx,
        })
        .map_err(|_| "depth writer worker exited".to_string())?;
    reply_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| "depth writer start acknowledgement timed out".to_string())??;

    UNBIND_FROM_END.store(config.unbind_from_end, Ordering::Release);
    CAPTURE_MAX_FRAMES.store(config.max_frames.unwrap_or(u64::MAX), Ordering::Release);
    *TARGET_DEPTH_HZ
        .lock()
        .map_err(|_| "target depth Hz lock poisoned".to_string())? = config.target_depth_hz;
    *CADENCE
        .lock()
        .map_err(|_| "cadence lock poisoned".to_string())? = None;
    LANE_FPS.store(config.lane_fps.unwrap_or(0), Ordering::Release);
    *PRESENT_RATE_GATE
        .lock()
        .map_err(|_| "present rate gate lock poisoned".to_string())? = None;
    RATE_VIOLATION_LOGGED.store(false, Ordering::Release);
    LANE_ABSENT_LOGGED.store(false, Ordering::Release);
    CAPTURE_ISSUED.store(0, Ordering::Release);
    CAPTURE_LIMIT_LOGGED.store(false, Ordering::Release);
    CADENCE_FIRST_DECISION_LOGGED.store(false, Ordering::Release);
    FORMAL_FIRST_PRESENT_LOGGED.store(false, Ordering::Release);
    SNAPSHOT_FIRST_ELIGIBLE_LOGGED.store(false, Ordering::Release);
    // 与上面几条同一批：逐 run 复位，否则第二个 run 起不再报告连续异常中止。
    HEALTH_ABORT_LATCHED.store(false, Ordering::Release);
    // 上一轮若在控制线程取走信号之前就被拆掉（例如同一刻来了 StopRun），
    // 信号会留在锁里。不清就会让**新 run 一起步就被上一轮的中止停掉**。
    *HEALTH_ABORT
        .lock()
        .map_err(|_| "depth frame health abort 锁已中毒".to_string())? = None;
    PRESENT_ONLY.store(config.present_only, Ordering::Release);
    WATERMARK_ONLY.store(config.watermark_only, Ordering::Release);
    WRITER_BYPASS.store(config.writer_bypass, Ordering::Release);
    if config.present_only {
        log(
            "⚠️ Present-only 基线档：保留 hook/QPC 计时，不写水印、不回读深度；本次运行不得用于交付",
        );
    } else if config.watermark_only {
        log("⚠️ 仅水印档：写水印但不回读深度。本次运行不产出深度帧，不得用于交付");
    }
    if config.writer_bypass {
        log(
            "⚠️ writer A/B 旁路档：保留 selected-Present 与 GPU 回读，在 EXR 编码前丢弃帧；本次运行不得用于交付",
        );
    }
    WRITER_QUEUE_DROPPED.store(0, Ordering::Release);
    WRITER_QUEUE_DEPTH.store(0, Ordering::Release);
    WRITER_BYPASSED.store(0, Ordering::Release);
    FIRST_PRESENT_SEQ.store(u64::MAX, Ordering::Release);
    clear_observed_swapchain_devices()?;
    D3D12_PRESENTATION_LAST_PRESENT.store(u64::MAX, Ordering::Release);
    if let Ok(mut error) = D3D12_LAST_ARM_ERROR.lock() {
        *error = None;
    }
    CAPTURE_THIS_FRAME.store(false, Ordering::Release);
    DRAINING.store(false, Ordering::Release);
    DRAIN_ERROR_LOGGED.store(false, Ordering::Release);
    ACTIVE_PROJECTION
        .lock()
        .map_err(|_| "run projection lock poisoned during start".to_string())?
        .start(generation, resolved_projection, performance_validation_only)?;
    *CAPTURE_WARMUP
        .lock()
        .map_err(|_| "capture warmup lock poisoned during start".to_string())? =
        defer_formal_capture.then(|| CaptureWarmup::new(generation));
    EXACT_PRODUCTION_SNAPSHOT_ENABLED.store(
        exact_production_snapshot_mode(config.camera_production.is_some(), pass_identity_trace),
        Ordering::Release,
    );
    FORMAL_CAPTURE_ARMED.store(!defer_formal_capture, Ordering::Release);
    if let Some(bootstrap) = D3D12_EARLY_BOOTSTRAP.get() {
        bootstrap
            .backend()
            .set_runtime_observation_active(d3d12_deep_observation_required(
                config.present_only,
                config.watermark_only,
            ));
    }
    Ok(())
}

fn stop_depth_writer(generation: u64) -> Result<DepthWriterSummary, String> {
    EXACT_PRODUCTION_SNAPSHOT_ENABLED.store(false, Ordering::Release);
    let (reply_tx, reply_rx) = sync_channel(1);
    DEPTH_TX
        .get()
        .ok_or("depth writer worker is not initialized")?
        .send(DepthWriterCommand::Stop {
            generation,
            reply: reply_tx,
        })
        .map_err(|_| "depth writer worker exited".to_string())?;
    let summary = reply_rx
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| "depth writer flush acknowledgement timed out".to_string())??;
    PASS_IDENTITY_TRACE_ENABLED.store(false, Ordering::Release);
    if WRITER_QUEUE_DEPTH.load(Ordering::Acquire) != 0 {
        return Err(format!(
            "writer queue is not empty after stop: {}",
            WRITER_QUEUE_DEPTH.load(Ordering::Acquire)
        ));
    }
    Ok(summary)
}

fn init_camera_production(
    config: &depth_lab::probe_config::ProbeConfig,
    root: std::path::PathBuf,
    performance_validation_only: bool,
) -> Result<(), String> {
    let production = config
        .camera_production
        .as_ref()
        .ok_or("camera_production 配置缺失")?;
    let profile_path = config
        .depth_profile_path
        .as_deref()
        .ok_or("Camera production 缺 depth_profile_path")?;
    let profile_bytes = match config.depth_profile_sha256.as_deref() {
        Some(expected) => read_verified_snapshot(
            std::path::Path::new(profile_path),
            expected,
            "depth profile",
        )?,
        None => std::fs::read(profile_path)
            .map_err(|error| format!("读 depth profile {profile_path} 失败: {error}"))?,
    };
    let profile: depth_lab::DepthProfile = serde_json::from_slice(&profile_bytes)
        .map_err(|error| format!("解析 depth profile {profile_path} 失败: {error}"))?;
    let binding_path = std::path::Path::new(&production.binding_path);
    let binding_bytes = match production.binding_sha256.as_deref() {
        Some(expected) => read_verified_snapshot(binding_path, expected, "CameraBinding")?,
        None => std::fs::read(binding_path).map_err(|error| {
            format!("读 CameraBinding {} 失败: {error}", binding_path.display())
        })?,
    };
    let binding: depth_lab::CameraBindingDocument = serde_json::from_slice(&binding_bytes)
        .map_err(|error| format!("parse CameraBinding: {error}"))?;
    let actual_executable_sha256 = current_executable_sha256()?;
    binding.validate_for_profile(&profile, &actual_executable_sha256)?;
    let context = depth_lab::CameraPoseStreamContext {
        identity: depth_lab::StreamIdentity {
            session_id: config.session_id.clone(),
            run_id: production.run_id.clone(),
            generation: production.generation,
        },
        dsv_id: profile.dsv.dsv_id.clone(),
    };
    match binding {
        depth_lab::CameraBindingDocument::V1(binding) => crate::camera_probe::init_production(
            root,
            binding,
            context,
            &actual_executable_sha256,
            performance_validation_only,
        ),
        depth_lab::CameraBindingDocument::V2(binding) => {
            let bootstrap = D3D12_EARLY_BOOTSTRAP
                .get()
                .ok_or("D3D12 camera requires the early launcher")?;
            bootstrap
                .backend()
                .configure_production_camera(binding.clone(), context.clone())?;
            if d3d12_deep_observation_required(config.present_only, config.watermark_only) {
                crate::camera_probe::init_d3d12_production(
                    root,
                    binding,
                    context,
                    &actual_executable_sha256,
                )
            } else {
                crate::camera_probe::init_d3d12_production_camera_only(
                    root,
                    binding,
                    context,
                    &actual_executable_sha256,
                )
            }
        }
        depth_lab::CameraBindingDocument::Kcd2Combined(binding) => {
            crate::kcd2_live_pose::initialize_with_binding(
                &root,
                depth_lab::StreamIdentity {
                    session_id: config.session_id.clone(),
                    run_id: production.run_id.clone(),
                    generation: production.generation,
                },
                &actual_executable_sha256,
                binding,
                &profile.dsv.dsv_id,
                performance_validation_only,
            )
            .and_then(|enabled| {
                enabled
                    .then_some(())
                    .ok_or_else(|| "KCD2 combined CameraBinding did not enable live pose".into())
            })
        }
    }
}

fn current_executable_sha256() -> Result<String, String> {
    if let Some(digest) = CURRENT_EXECUTABLE_SHA256.get() {
        return Ok(digest.clone());
    }
    let executable = std::env::current_exe()
        .map_err(|error| format!("定位当前游戏 executable 失败: {error}"))?;
    let digest = executable_sha256_at(&executable)?;
    let _ = CURRENT_EXECUTABLE_SHA256.set(digest.clone());
    Ok(CURRENT_EXECUTABLE_SHA256.get().cloned().unwrap_or(digest))
}

/// v3 replays external evidence at every phase boundary, so it must also re-open and re-hash the
/// loaded executable instead of inheriting the legacy production OnceLock snapshot.
fn fresh_current_executable_identity() -> Result<(std::path::PathBuf, String), String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("定位当前游戏 executable 失败: {error}"))?;
    let sha256 = executable_sha256_at(&executable)?;
    Ok((executable, sha256))
}

fn executable_sha256_at(path: &std::path::Path) -> Result<String, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("打开 game executable {} 失败: {error}", path.display()))?;
    sha256_stream(file)
        .map_err(|error| format!("读取 game executable {} 失败: {error}", path.display()))
}

fn sha256_stream(mut reader: impl std::io::Read) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        hasher.update(&chunk[..count]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn configure_depth_selection(
    config: &depth_lab::probe_config::ProbeConfig,
) -> Result<Option<depth_lab::DsvDiscoveryDescriptor>, String> {
    PROFILE_DSV_OPERATOR_EXACT_PIN.store(false, Ordering::Release);
    CAMERA_DISCOVERY_V3_EXACT_ACTIVE.store(false, Ordering::Release);
    *D3D12_DEPTH_PROFILE
        .lock()
        .map_err(|_| "D3D12 depth profile lock poisoned during run reset".to_string())? = None;
    D3D12_PAIRED_WARMUP_FRAMES.store(0, Ordering::Release);
    D3D12_FORMAL_START_PRESENT.store(u64::MAX, Ordering::Release);
    let selected_dsv_descriptor = validate_camera_discovery_selection(config)?;
    let strict_v3_selection = selected_dsv_descriptor.is_some();
    let expected = if let Some(discovery) = &config.camera_discovery {
        match selected_dsv_descriptor {
            Some(descriptor) => Some(DepthSelectionExpectation::ExactDescriptor(descriptor)),
            None => Some(DepthSelectionExpectation::LegacyRender(
                discovery
                    .render
                    .clone()
                    .ok_or("legacy camera discovery is missing render identity")?,
            )),
        }
    } else if let Some(profile_path) = config.depth_profile_path.as_deref() {
        let bytes = match config.depth_profile_sha256.as_deref() {
            Some(expected) => read_verified_snapshot(
                std::path::Path::new(profile_path),
                expected,
                "depth profile",
            )?,
            None => std::fs::read(profile_path)
                .map_err(|error| format!("读 depth profile {profile_path} 失败: {error}"))?,
        };
        let profile: depth_lab::DepthProfile = serde_json::from_slice(&bytes)
            .map_err(|error| format!("解析 depth profile {profile_path} 失败: {error}"))?;
        if profile.renderer_backend == depth_lab::RendererBackend::D3d12 {
            if profile.dsv.formats.is_empty()
                || profile.dsv.dsv_id.trim().is_empty()
                || profile.dsv.evidence_path.trim().is_empty()
            {
                return Err(
                    "D3D12 production requires exact candidate id, formats, and evidence".into(),
                );
            }
            let accepted_formats = profile
                .dsv
                .formats
                .iter()
                .map(|format| match format {
                    depth_lab::DiscoveryDepthFormat::R24G8Typeless => DXGI_FORMAT_R24G8_TYPELESS.0,
                    depth_lab::DiscoveryDepthFormat::D24UnormS8Uint => {
                        DXGI_FORMAT_D24_UNORM_S8_UINT.0
                    }
                    depth_lab::DiscoveryDepthFormat::R32Typeless => DXGI_FORMAT_R32_TYPELESS.0,
                    depth_lab::DiscoveryDepthFormat::D32Float => DXGI_FORMAT_D32_FLOAT.0,
                    depth_lab::DiscoveryDepthFormat::R32G8X24Typeless => {
                        DXGI_FORMAT_R32G8X24_TYPELESS.0
                    }
                    depth_lab::DiscoveryDepthFormat::D32FloatS8X24Uint => {
                        DXGI_FORMAT_D32_FLOAT_S8X24_UINT.0
                    }
                })
                .collect();
            *D3D12_DEPTH_PROFILE
                .lock()
                .map_err(|_| "D3D12 depth profile lock poisoned".to_string())? = Some((
                crate::d3d12_backend::D3d12DepthSelector {
                    candidate_id: profile.dsv.dsv_id.clone(),
                    width: profile.dsv.expected_width,
                    height: profile.dsv.expected_height,
                    sample_count: profile.dsv.sample_count,
                    accepted_formats,
                    stable_selection: profile.d3d12_selection.clone(),
                },
                profile.readback_slots,
            ));
        }
        PROFILE_DSV_OPERATOR_EXACT_PIN.store(
            !profile.dsv.formats.is_empty()
                && !profile.dsv.dsv_id.trim().is_empty()
                && !profile.dsv.evidence_path.trim().is_empty(),
            Ordering::Release,
        );
        let formats = if profile.dsv.formats.is_empty() {
            // Legacy profiles predate exact format pinning. Preserve their historical behavior.
            vec![
                depth_lab::DiscoveryDepthFormat::R24G8Typeless,
                depth_lab::DiscoveryDepthFormat::D24UnormS8Uint,
                depth_lab::DiscoveryDepthFormat::R32Typeless,
                depth_lab::DiscoveryDepthFormat::D32Float,
                depth_lab::DiscoveryDepthFormat::R32G8X24Typeless,
                depth_lab::DiscoveryDepthFormat::D32FloatS8X24Uint,
            ]
        } else {
            profile.dsv.formats.clone()
        };
        Some(DepthSelectionExpectation::LegacyRender(
            depth_lab::CameraDiscoveryRenderIdentity {
                width: profile.dsv.expected_width,
                height: profile.dsv.expected_height,
                sample_count: profile.dsv.sample_count,
                formats,
            },
        ))
    } else {
        None
    };
    *DEPTH_SELECTION
        .lock()
        .map_err(|_| "depth selection lock poisoned during run reset".to_string())? =
        DepthSelectionState {
            expected,
            selected_pair: None,
            retired_pairs: BTreeSet::new(),
            pending_explicit_unbind: None,
            ambiguous: false,
        };
    CAMERA_DISCOVERY_V3_EXACT_ACTIVE.store(strict_v3_selection, Ordering::Release);
    Ok(selected_dsv_descriptor)
}

fn validate_camera_discovery_selection(
    config: &depth_lab::probe_config::ProbeConfig,
) -> Result<Option<depth_lab::DsvDiscoveryDescriptor>, String> {
    let Some(discovery) = config.camera_discovery.as_ref() else {
        return Ok(None);
    };
    let Some(selection) = discovery.dsv_selection.as_ref() else {
        return Ok(None);
    };
    let evidence_root = std::path::Path::new(&selection.evidence_root);
    let attestation_path = evidence_root.join(&selection.attestation_locator);
    let validated = depth_lab::validate_dsv_selection_attestation_with_sources(
        evidence_root,
        &attestation_path,
        &selection.attestation_sha256,
    )?;
    if validated.locator != selection.attestation_locator
        || validated.exact_bytes_sha256 != selection.attestation_sha256
    {
        return Err("camera discovery DSV attestation locator/hash changed during replay".into());
    }
    let (executable, actual_executable_sha256) = fresh_current_executable_identity()?;
    let actual_basename = executable
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("current executable basename is not UTF-8")?;
    validate_camera_discovery_executable_binding(
        &validated.target.executable.basename,
        &validated.target.executable.sha256,
        &discovery.executable_sha256,
        actual_basename,
        &actual_executable_sha256,
    )?;
    validate_d3d11_copy_resource_envelope(validated.selected_descriptor)?;
    Ok(Some(validated.selected_descriptor))
}

fn executable_basename_matches(attested: &str, actual: &str) -> bool {
    !attested.is_empty()
        && attested.is_ascii()
        && !attested.bytes().any(|byte| matches!(byte, b'/' | b'\\'))
        && attested.eq_ignore_ascii_case(actual)
}

fn validate_camera_discovery_executable_binding(
    attested_basename: &str,
    attested_sha256: &str,
    config_sha256: &str,
    actual_basename: &str,
    actual_sha256: &str,
) -> Result<(), String> {
    if !executable_basename_matches(attested_basename, actual_basename)
        || attested_sha256 != config_sha256
        || attested_sha256 != actual_sha256
    {
        return Err(format!(
            "camera discovery DSV attestation executable drift: attestation_basename={attested_basename} actual_basename={actual_basename} attestation_hash={attested_sha256} config_hash={config_sha256} actual_hash={actual_sha256}"
        ));
    }
    Ok(())
}

fn validate_d3d11_copy_resource_envelope(
    descriptor: depth_lab::DsvDiscoveryDescriptor,
) -> Result<(), String> {
    let view_dimension_supported = matches!(
        descriptor.view_dimension,
        value if value == D3D11_DSV_DIMENSION_TEXTURE2D.0
            || value == D3D11_DSV_DIMENSION_TEXTURE2DARRAY.0
    );
    let view_family = d3d11_depth_format_family(descriptor.view_format);
    let resource_family = d3d11_depth_format_family(descriptor.resource_format);
    let typed_view = matches!(
        descriptor.view_format,
        value if value == DXGI_FORMAT_D24_UNORM_S8_UINT.0
            || value == DXGI_FORMAT_D32_FLOAT.0
            || value == DXGI_FORMAT_D32_FLOAT_S8X24_UINT.0
    );
    let admitted_bind_flags =
        D3D11_BIND_DEPTH_STENCIL.0 as u32 | D3D11_BIND_SHADER_RESOURCE.0 as u32;
    if !view_dimension_supported
        || descriptor.view_flags & !0x3 != 0
        || descriptor.mip_slice != 0
        || descriptor.first_array_slice != 0
        || descriptor.view_array_size != 1
        || descriptor.width == 0
        || descriptor.height == 0
        || descriptor.resource_mip_levels != 1
        || descriptor.resource_array_size != 1
        || descriptor.sample_count != 1
        || descriptor.sample_quality != 0
        || descriptor.usage != D3D11_USAGE_DEFAULT.0
        || descriptor.cpu_access_flags != 0
        || descriptor.misc_flags != 0
        || descriptor.bind_flags & D3D11_BIND_DEPTH_STENCIL.0 as u32 == 0
        || descriptor.bind_flags & !admitted_bind_flags != 0
        || !typed_view
        || view_family.is_none()
        || view_family != resource_family
    {
        return Err(format!(
            "selected D3D11 descriptor is outside the admitted CopyResource envelope: {descriptor:?}"
        ));
    }
    Ok(())
}

fn d3d11_depth_format_family(format: i32) -> Option<u8> {
    match format {
        value
            if value == DXGI_FORMAT_R24G8_TYPELESS.0
                || value == DXGI_FORMAT_D24_UNORM_S8_UINT.0 =>
        {
            Some(1)
        }
        value if value == DXGI_FORMAT_R32_TYPELESS.0 || value == DXGI_FORMAT_D32_FLOAT.0 => Some(2),
        value
            if value == DXGI_FORMAT_R32G8X24_TYPELESS.0
                || value == DXGI_FORMAT_D32_FLOAT_S8X24_UINT.0 =>
        {
            Some(3)
        }
        _ => None,
    }
}

fn depth_format_matches(
    expected: &[depth_lab::DiscoveryDepthFormat],
    actual: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> bool {
    expected.iter().any(|format| match format {
        depth_lab::DiscoveryDepthFormat::R24G8Typeless => actual == DXGI_FORMAT_R24G8_TYPELESS,
        depth_lab::DiscoveryDepthFormat::D24UnormS8Uint => actual == DXGI_FORMAT_D24_UNORM_S8_UINT,
        depth_lab::DiscoveryDepthFormat::R32Typeless => actual == DXGI_FORMAT_R32_TYPELESS,
        depth_lab::DiscoveryDepthFormat::D32Float => actual == DXGI_FORMAT_D32_FLOAT,
        depth_lab::DiscoveryDepthFormat::R32G8X24Typeless => {
            actual == DXGI_FORMAT_R32G8X24_TYPELESS
        }
        depth_lab::DiscoveryDepthFormat::D32FloatS8X24Uint => {
            actual == DXGI_FORMAT_D32_FLOAT_S8X24_UINT
        }
    })
}

fn depth_format_value_matches(expected: &[depth_lab::DiscoveryDepthFormat], actual: i32) -> bool {
    depth_format_matches(
        expected,
        windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(actual),
    )
}

#[cfg(test)]
fn is_selected_depth_desc(
    desc: &D3D11_TEXTURE2D_DESC,
    expected: &depth_lab::CameraDiscoveryRenderIdentity,
) -> bool {
    desc.Width == expected.width
        && desc.Height == expected.height
        && desc.SampleDesc.Count == expected.sample_count
        && depth_format_matches(&expected.formats, desc.Format)
}

fn selected_depth_after_observed_unbind(
    depth_stencil_view: *mut core::ffi::c_void,
    expected_reacquire: Option<DsvPairIdentity>,
    discovery_guard: Option<&crate::camera_probe::DiscoveryCallbackGuard>,
) -> Result<Option<SelectedDepth>, String> {
    let result = selected_depth_after_observed_unbind_inner(
        depth_stencil_view,
        expected_reacquire,
        discovery_guard,
    );
    map_selected_depth_result(result, || {
        // The inner call has returned, so any COM temporaries and selection guard have already
        // dropped. Re-locking here cannot deadlock the pre-inspection/selection-lock error path.
        latch_exact_depth_selection_ambiguity(discovery_guard);
    })
}

fn map_selected_depth_result<T>(
    result: Result<T, String>,
    latch_error: impl FnOnce(),
) -> Result<T, String> {
    if result.is_err() {
        latch_error();
    }
    result
}

fn selected_depth_after_observed_unbind_inner(
    depth_stencil_view: *mut core::ffi::c_void,
    expected_reacquire: Option<DsvPairIdentity>,
    discovery_guard: Option<&crate::camera_probe::DiscoveryCallbackGuard>,
) -> Result<Option<SelectedDepth>, String> {
    let dsv = unsafe { ID3D11DepthStencilView::from_raw_borrowed(&depth_stencil_view) }
        .ok_or("DSV 指针无效")?;
    let resource = unsafe { dsv.GetResource() }.map_err(|e| format!("DSV GetResource: {e}"))?;
    let texture: ID3D11Texture2D = resource
        .cast()
        .map_err(|e| format!("DSV resource 不是 Texture2D: {e}"))?;
    let observed = inspect_dsv_resource(depth_stencil_view)?;
    let mut selection = DEPTH_SELECTION
        .lock()
        .map_err(|_| "depth selection lock poisoned".to_string())?;
    let was_ambiguous = selection.ambiguous;
    match accept_reacquired_depth_resource(&mut selection, &observed, expected_reacquire) {
        Ok(false) => Ok(None),
        Ok(true) => Ok(Some(SelectedDepth {
            view: dsv.clone(),
            texture,
            descriptor: observed.descriptor,
            identity: selection.identity(DsvPairIdentity {
                resource_tag: observed.resource_tag,
                view_tag: observed.view_tag,
            }),
        })),
        Err(error) => {
            if !was_ambiguous && let Some(guard) = discovery_guard {
                crate::camera_probe::mark_discovery_dsv_ambiguous(guard);
            }
            Err(error)
        }
    }
}

fn accept_reacquired_depth_resource(
    selection: &mut DepthSelectionState,
    observed: &ObservedDsvResource,
    expected_reacquire: Option<DsvPairIdentity>,
) -> Result<bool, String> {
    let Some(expected) = selection.expected.as_ref() else {
        return Ok(false);
    };
    let matches = match expected {
        DepthSelectionExpectation::LegacyRender(expected) => {
            observed.descriptor.width == expected.width
                && observed.descriptor.height == expected.height
                && observed.descriptor.sample_count == expected.sample_count
                && depth_format_value_matches(
                    &expected.formats,
                    observed.descriptor.resource_format,
                )
        }
        DepthSelectionExpectation::ExactDescriptor(expected) => observed.descriptor == *expected,
    };
    if !matches {
        return Ok(false);
    }
    let pair = DsvPairIdentity {
        resource_tag: observed.resource_tag,
        view_tag: observed.view_tag,
    };
    if matches!(expected, DepthSelectionExpectation::LegacyRender(_)) {
        // Resource-level aliases are safe only inside the single-subresource CopyResource
        // envelope. Read-only depth/stencil flags may differ; mip/array slices must not.
        if let Err(error) = validate_d3d11_copy_resource_envelope(observed.descriptor) {
            selection.ambiguous = true;
            return Err(error);
        }
    }
    if let Some(expected) = expected_reacquire
        && !selection.identity(expected).matches_pair(pair)
    {
        selection.ambiguous = true;
        return Err(format!(
            "phase reset DSV reacquire exact descriptor matched an unexpected pair: expected=({:#x},{:#x}) actual=({:#x},{:#x})",
            expected.resource_tag, expected.view_tag, pair.resource_tag, pair.view_tag,
        ));
    }
    selection.accept_matching_pair(pair)?;
    Ok(true)
}

#[cfg(test)]
fn accept_selected_depth_resource(
    selection: &mut DepthSelectionState,
    observed: &ObservedDsvResource,
) -> Result<bool, String> {
    accept_reacquired_depth_resource(selection, observed, None)
}

fn record_selected_dsv_unbind(present_seq: u64) -> Result<u32, String> {
    let mut counter = SELECTED_DSV_UNBINDS
        .lock()
        .map_err(|_| "selected DSV unbind counter lock poisoned".to_string())?;
    if counter.present_seq != present_seq {
        counter.present_seq = present_seq;
        counter.count = 0;
    }
    counter.count = counter
        .count
        .checked_add(1)
        .ok_or("selected DSV unbind count overflow")?;
    Ok(counter.count)
}

fn selected_dsv_unbind_total(present_seq: u64) -> Result<u32, String> {
    let counter = SELECTED_DSV_UNBINDS
        .lock()
        .map_err(|_| "selected DSV unbind counter lock poisoned".to_string())?;
    if counter.present_seq != present_seq {
        return Err(format!(
            "present_seq {present_seq} has no selected DSV unbind evidence"
        ));
    }
    Ok(counter.count)
}

fn exact_snapshot_copy_decision(
    identity: Option<crate::camera_probe::MainPassIdentity>,
    source_resource_tag: u64,
    captured: Option<ExactSnapshotRecord>,
) -> Result<bool, String> {
    let Some(identity) = identity else {
        return Ok(false);
    };
    if identity.exact_draw_ordinal.is_none() || !identity.projection_observed {
        return Ok(false);
    }
    if let Some(captured) = captured {
        if captured.identity == identity && captured.source_resource_tag == source_resource_tag {
            // D3D11 permits redundant OM bindings. The same resource and exact draw identity is
            // the same evidence claim, not a second candidate. A different resource or identity
            // remains ambiguous and must fail closed.
            return Ok(false);
        }
        return Err(format!(
            "multiple selected DSV passes claim the exact production projection draw: previous_identity={:?} previous_resource=0x{:x} incoming_identity={identity:?} incoming_resource=0x{source_resource_tag:x}",
            captured.identity, captured.source_resource_tag
        ));
    }
    Ok(true)
}

fn select_exact_snapshot(
    record: Option<ExactSnapshotRecord>,
    ambiguous: bool,
    unbind_total: u32,
    expected: Option<crate::camera_probe::ExactProductionDrawIdentity>,
) -> Result<SelectedExactSnapshot, String> {
    if ambiguous {
        return Err("exact production DSV snapshot identity is ambiguous".into());
    }
    let expected =
        expected.ok_or("exact production projection draw was not observed this frame")?;
    if !expected.projection_observed {
        return Err("exact production projection draw lacks observed_this_frame evidence".into());
    }
    let record = record.ok_or("exact production projection pass was not snapshotted this frame")?;
    if record.identity.pass_ordinal != expected.pass_ordinal
        || record.identity.exact_draw_ordinal != Some(expected.draw_ordinal)
        || !record.identity.projection_observed
    {
        return Err(format!(
            "exact production DSV snapshot identity mismatch: snapshot={:?} expected={expected:?}",
            record.identity
        ));
    }
    if record.unbind_ordinal_from_start == 0 || record.unbind_ordinal_from_start > unbind_total {
        return Err(format!(
            "exact production DSV snapshot unbind {} is outside total {unbind_total}",
            record.unbind_ordinal_from_start
        ));
    }
    Ok(SelectedExactSnapshot {
        texture_index: record.texture_index,
        unbind_ordinal_from_end: unbind_total - record.unbind_ordinal_from_start + 1,
    })
}

fn snapshot_outgoing_selected(
    context: *mut core::ffi::c_void,
    incoming: Option<&SelectedDepth>,
) -> Result<(), String> {
    let outgoing = CURRENT_SELECTED_DEPTH
        .lock()
        .map_err(|_| "主场景 DSV 锁已中毒")?
        .as_ref()
        .cloned();
    let Some(outgoing) = outgoing else {
        return Ok(());
    };
    if incoming.is_some_and(|next| next.identity == outgoing.identity) {
        return Ok(());
    }
    let reobserved = inspect_dsv_resource(outgoing.view.as_raw())?;
    if reobserved.descriptor != outgoing.descriptor
        || reobserved.resource_tag != outgoing.pair().resource_tag
        || reobserved.view_tag != outgoing.pair().view_tag
    {
        return Err("selected DSV view/resource descriptor changed before CopyResource".into());
    }
    let texture = outgoing.texture.clone();

    // Between Present N and Present N+1, PRESENT_SEQ already contains N+1. Count every selected-DSV
    // transition for durable pass evidence, but normal exact-build production copies only the pass
    // that observed the configured projection draw. Diagnostic tracing keeps the old full ring.
    let present_seq = PRESENT_SEQ.load(Ordering::Relaxed);
    if !depth_candidate_capture_requested_now() {
        return Ok(());
    }
    if !SNAPSHOT_FIRST_ELIGIBLE_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "first eligible outgoing DSV snapshot: present_seq={present_seq} exact_mode={} incoming_present={}",
            exact_production_snapshot_enabled(),
            incoming.is_some()
        ));
    }
    let source_resource_tag = texture.as_raw() as usize as u64;
    let exact_mode = exact_production_snapshot_enabled();
    let trace_mode = PASS_IDENTITY_TRACE_ENABLED.load(Ordering::Relaxed);
    let pass_identity = (exact_mode || trace_mode)
        .then(|| crate::camera_probe::current_main_pass_identity(present_seq))
        .flatten();
    if exact_mode && !exact_snapshot_copy_decision(pass_identity, source_resource_tag, None)? {
        // Unrelated selected-DSV transitions remain part of the physical unbind audit. Only an
        // exact duplicate claim below is idempotent in both snapshot and unbind ledgers.
        record_selected_dsv_unbind(present_seq)?;
        return Ok(());
    }
    let mut unbind_ordinal_from_start = (!exact_mode)
        .then(|| record_selected_dsv_unbind(present_seq))
        .transpose()?;

    let device = unsafe { texture.GetDevice() }.map_err(|e| format!("depth GetDevice: {e}"))?;
    let ctx = unsafe { ID3D11DeviceContext::from_raw_borrowed(&context) }
        .ok_or("OMSetRenderTargets context 指针无效")?;
    let mut source_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut source_desc) };
    let snapshot_format = shader_readable_snapshot_format(source_desc.Format)?;

    let mut snapshot_guard = COMPLETED_DEPTH_SNAPSHOTS
        .lock()
        .map_err(|_| "最终 DSV 快照锁已中毒")?;
    let pool_len = if exact_mode { 1 } else { 12 };
    let needs_rebuild = snapshot_guard.as_ref().is_none_or(|snapshots| {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { snapshots.textures[0].0.GetDesc(&mut desc) };
        snapshots.textures.len() != pool_len
            || desc.Width != source_desc.Width
            || desc.Height != source_desc.Height
            || desc.Format != snapshot_format
    });
    if needs_rebuild {
        let snapshot_desc = D3D11_TEXTURE2D_DESC {
            MipLevels: 1,
            ArraySize: 1,
            Usage: D3D11_USAGE_DEFAULT,
            // GPU 池化（readback::capture 的 CS 降采样）要从快照建 SRV。快照格式
            // 统一归一成 TYPELESS 家族；CopyResource 允许同一 TYPELESS group 内从 typed
            // DSV 拷到 typeless snapshot。ETS2 实测源是 D32_FLOAT_S8X24_UINT，不能假定
            // 引擎原资源本身一定 typeless。
            Format: snapshot_format,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            ..source_desc
        };
        let mut textures = Vec::with_capacity(pool_len);
        for _ in 0..pool_len {
            let mut snapshot = None;
            unsafe { device.CreateTexture2D(&snapshot_desc, None, Some(&mut snapshot)) }
                .map_err(|e| format!("创建最终 DSV GPU 快照失败: {e}"))?;
            textures.push(DepthTexture(
                snapshot.ok_or("CreateTexture2D 返回空的最终 DSV 快照")?,
            ));
        }
        *snapshot_guard = Some(CompletedDepthSnapshots {
            textures,
            pass_draw_calls: vec![0; pool_len],
            pass_identities: vec![None; pool_len],
            next: 0,
            present_seq,
            copies_this_frame: 0,
            exact_snapshot: None,
            exact_snapshot_source: None,
            exact_snapshot_ambiguous: false,
        });
    }

    let snapshots = snapshot_guard.as_mut().expect("最终 DSV 快照刚创建");
    if snapshots.present_seq != present_seq {
        snapshots.present_seq = present_seq;
        snapshots.copies_this_frame = 0;
        snapshots.exact_snapshot = None;
        snapshots.exact_snapshot_source = None;
        snapshots.exact_snapshot_ambiguous = false;
    }
    if exact_mode {
        if let Some(captured) = snapshots.exact_snapshot {
            let source_is_held = snapshots
                .exact_snapshot_source
                .as_ref()
                .is_some_and(|source| {
                    source.texture.as_raw() as usize as u64 == captured.source_resource_tag
                });
            if !source_is_held {
                snapshots.exact_snapshot_ambiguous = true;
                return Err("exact production DSV source identity owner is missing".into());
            }
        }
        match exact_snapshot_copy_decision(
            pass_identity,
            source_resource_tag,
            snapshots.exact_snapshot,
        ) {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(error) => {
                snapshots.exact_snapshot_ambiguous = true;
                return Err(error);
            }
        }
        unbind_ordinal_from_start = Some(record_selected_dsv_unbind(present_seq)?);
    }
    let write_index = snapshots.next;
    let snapshot = snapshots.textures[write_index].0.clone();
    let dst: ID3D11Resource = snapshot.into();
    let src: ID3D11Resource = texture.clone().into();
    let _perf = crate::perf::scope(crate::perf::Section::DsvSnapshot);
    unsafe { ctx.CopyResource(&dst, &src) };

    snapshots.next = (write_index + 1) % snapshots.textures.len();
    snapshots.pass_draw_calls[write_index] = SELECTED_PASS_DRAW_CALLS.swap(0, Ordering::AcqRel);
    snapshots.pass_identities[write_index] = pass_identity;
    if exact_mode {
        snapshots.exact_snapshot = pass_identity.map(|identity| ExactSnapshotRecord {
            texture_index: write_index,
            unbind_ordinal_from_start: unbind_ordinal_from_start
                .expect("exact snapshot copy records one unbind"),
            source_resource_tag,
            identity,
        });
        snapshots.exact_snapshot_source = Some(outgoing);
    }
    snapshots.copies_this_frame += 1;
    Ok(())
}

fn shader_readable_snapshot_format(
    source: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
) -> Result<windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, String> {
    match source {
        f if f == DXGI_FORMAT_R24G8_TYPELESS || f == DXGI_FORMAT_D24_UNORM_S8_UINT => {
            Ok(DXGI_FORMAT_R24G8_TYPELESS)
        }
        f if f == DXGI_FORMAT_R32_TYPELESS || f == DXGI_FORMAT_D32_FLOAT => {
            Ok(DXGI_FORMAT_R32_TYPELESS)
        }
        f if f == DXGI_FORMAT_R32G8X24_TYPELESS || f == DXGI_FORMAT_D32_FLOAT_S8X24_UINT => {
            Ok(DXGI_FORMAT_R32G8X24_TYPELESS)
        }
        other => Err(format!(
            "depth snapshot 格式 {other:?} 不属于受支持的 TYPELESS 深度 family"
        )),
    }
}

fn build_pass_identity_trace(
    snapshots: &CompletedDepthSnapshots,
    generation: u64,
    present_seq: u64,
    selected_from_end: u32,
) -> PassIdentityTrace {
    build_pass_identity_trace_with_exact(
        snapshots,
        generation,
        present_seq,
        selected_from_end,
        crate::camera_probe::exact_production_draw_identity(present_seq),
    )
}

fn build_pass_identity_trace_with_exact(
    snapshots: &CompletedDepthSnapshots,
    generation: u64,
    present_seq: u64,
    selected_from_end: u32,
    exact: Option<crate::camera_probe::ExactProductionDrawIdentity>,
) -> PassIdentityTrace {
    let available = snapshots.copies_this_frame.min(snapshots.textures.len());
    let mut candidates = Vec::with_capacity(available);
    for ordinal_from_start in 1..=available {
        let index = (snapshots.next + snapshots.textures.len() - available + ordinal_from_start
            - 1)
            % snapshots.textures.len();
        let identity = snapshots.pass_identities[index];
        candidates.push(SnapshotPassIdentityTrace {
            unbind_ordinal_from_start: ordinal_from_start as u32,
            unbind_ordinal_from_end: (available - ordinal_from_start + 1) as u32,
            pass_ordinal: identity.map(|identity| identity.pass_ordinal),
            draw_calls: snapshots.pass_draw_calls[index],
            exact_draw_ordinal: identity.and_then(|identity| identity.exact_draw_ordinal),
            projection_observed: identity.is_some_and(|identity| identity.projection_observed),
        });
    }

    let join = classify_pass_identity_join(exact, selected_from_end, &candidates);

    PassIdentityTrace {
        schema: PASS_IDENTITY_TRACE_SCHEMA_V1,
        generation,
        present_seq,
        expected_pass_ordinal: exact.map(|identity| identity.pass_ordinal),
        expected_draw_ordinal: exact.map(|identity| identity.draw_ordinal),
        exact_draw_seen: exact.is_some(),
        projection_observed: exact.is_some_and(|identity| identity.projection_observed),
        selected_unbind_ordinal_from_end: selected_from_end,
        snapshots: candidates,
        join,
    }
}

fn classify_pass_identity_join(
    exact: Option<crate::camera_probe::ExactProductionDrawIdentity>,
    selected_from_end: u32,
    candidates: &[SnapshotPassIdentityTrace],
) -> PassIdentityJoin {
    let matching_projection = exact.map_or_else(Vec::new, |exact| {
        candidates
            .iter()
            .filter(|candidate| {
                candidate.pass_ordinal == Some(exact.pass_ordinal)
                    && candidate.exact_draw_ordinal == Some(exact.draw_ordinal)
                    && candidate.projection_observed
            })
            .collect::<Vec<_>>()
    });
    let selected = candidates
        .iter()
        .find(|candidate| candidate.unbind_ordinal_from_end == selected_from_end);
    match exact {
        None => PassIdentityJoin::ExactDrawMissing,
        Some(exact) if !exact.projection_observed => PassIdentityJoin::ProjectionObservationMissing,
        Some(_) if matching_projection.is_empty() => PassIdentityJoin::ProjectionPassNotSnapshotted,
        Some(_) if matching_projection.len() != 1 => PassIdentityJoin::Ambiguous,
        Some(_) if selected == matching_projection.first().copied() => PassIdentityJoin::Exact,
        Some(_) => PassIdentityJoin::SelectedSnapshotDiffers,
    }
}

fn capture_last_completed_at_present(
    present_seq: u64,
    callback_generation: u64,
    selection: Option<depth_lab::DsvSelectionAudit>,
) -> Result<(), String> {
    // 读的是**本帧**的结论——它是上一次 Present 末尾写进去的，
    // 本次 Present 末尾才会被下一帧的结论覆盖。顺序是 load-bearing。
    let selected_present_mode = selected_present_accepting_candidates();
    if !depth_candidate_capture_requested_now() {
        return Ok(());
    }
    let issued = CAPTURE_ISSUED.load(Ordering::Relaxed);
    if !selected_present_mode && !capture_budget_available(issued, capture_max_frames()) {
        if !CAPTURE_LIMIT_LOGGED.swap(true, Ordering::Relaxed) {
            log(&format!("P1c capture limit reached after {issued} frames"));
        }
        return Ok(());
    }
    let exact_mode = exact_production_snapshot_enabled();
    let exact_unbind_total = exact_mode
        .then(|| selected_dsv_unbind_total(present_seq))
        .transpose()?;
    let configured_from_end = UNBIND_FROM_END.load(Ordering::Acquire);
    let from_end = if configured_from_end == 0 {
        (present_seq as usize / 2) % 12 + 1
    } else {
        configured_from_end
    };
    let snapshot_guard = COMPLETED_DEPTH_SNAPSHOTS
        .lock()
        .map_err(|_| "最终 DSV 快照锁已中毒")?;
    let snapshots = snapshot_guard
        .as_ref()
        .ok_or_else(|| format!("present_seq {present_seq} has no completed DSV snapshot pool"))?;
    if snapshots.present_seq != present_seq || snapshots.copies_this_frame == 0 {
        if exact_mode {
            return Err(format!(
                "present_seq {present_seq} has no exact production DSV snapshot"
            ));
        }
        return Ok(());
    }
    let (index, unbind_ordinal_from_end, unbind_total, identity_trace) = if exact_mode {
        let unbind_total = exact_unbind_total.expect("exact mode unbind count checked above");
        let selected = select_exact_snapshot(
            snapshots.exact_snapshot,
            snapshots.exact_snapshot_ambiguous,
            unbind_total,
            crate::camera_probe::exact_production_draw_identity(present_seq),
        )?;
        if present_seq.is_multiple_of(300) {
            log(&format!(
                "P1c exact snapshot present_seq={present_seq} unbind_total={unbind_total} from_end={}",
                selected.unbind_ordinal_from_end
            ));
        }
        (
            selected.texture_index,
            selected.unbind_ordinal_from_end,
            unbind_total,
            None,
        )
    } else {
        if present_seq.is_multiple_of(300) {
            log(&format!(
                "P1c unbinds present_seq={present_seq} count={} from_end={from_end}",
                snapshots.copies_this_frame
            ));
        }
        let available = snapshots.copies_this_frame.min(snapshots.textures.len());
        let Some(index) = snapshot_index(
            snapshots.next,
            snapshots.textures.len(),
            available,
            from_end,
        ) else {
            return Ok(());
        };
        let unbind_total = u32::try_from(snapshots.copies_this_frame)
            .map_err(|_| "legacy DSV snapshot count exceeds u32".to_string())?;
        let unbind_ordinal_from_end =
            u32::try_from(from_end).map_err(|_| "legacy from_end exceeds u32".to_string())?;
        let identity_trace = PASS_IDENTITY_TRACE_ENABLED
            .load(Ordering::Acquire)
            .then(|| {
                build_pass_identity_trace(
                    snapshots,
                    callback_generation,
                    present_seq,
                    unbind_ordinal_from_end,
                )
            });
        (index, unbind_ordinal_from_end, unbind_total, identity_trace)
    };
    let texture = snapshots.textures[index].0.clone();
    let draw_calls = snapshots.pass_draw_calls[index];
    drop(snapshot_guard);
    let selection = selection.ok_or_else(|| {
        format!(
            "present_seq {present_seq} has a depth snapshot but no complete DSV ranking evidence"
        )
    })?;
    if !selection.selected_matches_top && !DSV_RANK_MISMATCH_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "DSV ranking warning: selected={} ranked_top={:?}; operator selection retained but evidence is marked (#577)",
            selection.selected_id, selection.ranked_top_id
        ));
    }
    if !selection.ranking_confident && !DSV_RANK_AMBIGUOUS_LOGGED.swap(true, Ordering::AcqRel) {
        log(&format!(
            "DSV ranking fail-closed: score margin {:?} is below {} (#577)",
            selection.score_margin,
            depth_lab::DsvRanking::CLEAR_MARGIN
        ));
    }
    if selection.any_msaa && !DSV_RANK_MSAA_LOGGED.swap(true, Ordering::AcqRel) {
        log(
            "DSV ranking fail-closed: frame contains an MSAA depth target; typeless path is required (#577)",
        );
    }
    let pass = depth_lab::capture_timing::DsvPassEvidence {
        unbind_ordinal_from_end,
        unbind_total,
        draw_calls,
        selection,
    };
    let device = unsafe { texture.GetDevice() }.map_err(|e| format!("snapshot GetDevice: {e}"))?;
    let ctx: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
        .map_err(|e| format!("拿不到 immediate context: {e}"))?;
    // 降采样方向必须与 Z 约定一致，否则每条物体边缘都系统性地取到最远表面。
    // profile 缺失时保守取 Traditional 并已在 resolved_projection 里记过一行日志。
    let resolved = active_projection(callback_generation)?;
    let conv = resolved.map_or(ZConvention::Traditional, |value| value.conv);
    let readback_slots = resolved.map_or(DEFAULT_READBACK_SLOTS, |value| value.readback_slots);
    let cfg = CaptureConfig {
        dst_w: DELIVERY_WIDTH,
        dst_h: DELIVERY_HEIGHT,
        conv,
        slots: readback_slots,
        deadline_ticks: READBACK_DEADLINE_FRAMES,
    };
    let _generation_guard = DEPTH_SEND_LOCK
        .try_lock()
        .map_err(|_| "late depth capture raced with writer stop".to_string())?;
    if !depth_capture_enabled()
        || callback_generation == 0
        || ACTIVE_GENERATION.load(Ordering::Acquire) != callback_generation
    {
        return Ok(());
    }
    if selected_present_mode {
        let _candidate_perf = crate::perf::scope(crate::perf::Section::CandidateMaterialize);
        let projection = if ACTIVE_CONFIG
            .lock()
            .map_err(|_| "active config lock poisoned during candidate materialization")?
            .as_ref()
            .is_some_and(|config| config.camera_production.is_some())
        {
            observed_index_projection(callback_generation, present_seq)?
        } else {
            active_projection(callback_generation)?.map_or_else(
                depth_lab::capture_timing::IndexProjection::unknown,
                |value| value.index,
            )
        };
        let mut runtime_guard = SELECTED_PRESENT_RUNTIME
            .lock()
            .map_err(|_| "selected-Present runtime lock poisoned")?;
        let runtime = runtime_guard
            .as_mut()
            .ok_or("selected-Present mode is enabled without a runtime")?;
        let slot = runtime
            .ring_state
            .materialize(present_seq)
            .map_err(|fault| {
                latch_selected_present_fault(fault.reason, fault.to_string());
                fault.to_string()
            })?;
        if runtime.gpu_ring.is_none() {
            match readback::GpuCandidateRing::new(
                &device,
                &texture,
                runtime.bootstrap.ring_capacity,
                runtime.bootstrap.width as usize,
                runtime.bootstrap.height as usize,
                conv,
                runtime.bootstrap.vram_limit_bytes,
                runtime.bootstrap.vram_reserve_bytes,
                runtime.readback_slots,
                READBACK_DEADLINE_FRAMES,
            ) {
                Ok(ring) => {
                    let stats = ring.stats();
                    log(&format!(
                        "selected-Present GPU ring allocated capacity={} staging_slots={} texture_bytes={} estimated_bytes={} limit={} reserve={} budget_before={} usage_before={} headroom_before={} budget_after={} usage_after={} headroom_after={} actual_delta={}",
                        stats.capacity,
                        runtime.readback_slots,
                        stats.texture_bytes,
                        stats.estimated_bytes,
                        stats.configured_limit_bytes,
                        stats.configured_reserve_bytes,
                        stats.budget_before,
                        stats.usage_before,
                        stats.headroom_before,
                        stats.budget_after,
                        stats.usage_after,
                        stats.headroom_after,
                        stats.actual_usage_delta,
                    ));
                    runtime.candidate_conv = Some(conv);
                    runtime.gpu_ring = Some(ring);
                }
                Err(error) => {
                    let reason = if error.contains("VRAM")
                        || error.contains("headroom")
                        || error.contains("video-memory budget")
                    {
                        depth_lab::selected_present_feedback::FaultReason::VramBudgetInsufficient
                    } else {
                        depth_lab::selected_present_feedback::FaultReason::CandidateAllocationFailed
                    };
                    latch_selected_present_fault(reason, &error);
                    return Err(error);
                }
            }
        }
        if runtime.candidate_conv != Some(conv) {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::ResourceEpochMismatch,
                "depth convention changed after selected-Present resources were created",
            );
            return Err(
                "depth convention changed after selected-Present resources were created".into(),
            );
        }
        if let Err(error) = runtime
            .gpu_ring
            .as_mut()
            .expect("candidate ring initialized above")
            .materialize(&device, &ctx, &texture, slot)
        {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::ResourceEpochMismatch,
                &error,
            );
            return Err(error);
        }
        if runtime
            .candidate_evidence
            .insert(
                present_seq,
                PendingCaptureEvidence {
                    pass,
                    identity_trace,
                    projection: Some(projection),
                },
            )
            .is_some()
        {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::RingIdentityMismatch,
                format!("duplicate candidate evidence for present_seq {present_seq}"),
            );
            return Err(format!(
                "duplicate candidate evidence for present_seq {present_seq}"
            ));
        }
        return Ok(());
    }
    let accepted = readback::capture(&device, &ctx, &texture, present_seq, cfg)?;
    if accepted {
        crate::perf::mark_frame(crate::perf::FrameFlag::Capture);
        let replaced = CAPTURE_PASS_EVIDENCE
            .lock()
            .map_err(|_| "DSV pass evidence lock poisoned".to_string())?
            .insert(
                present_seq,
                PendingCaptureEvidence {
                    pass,
                    identity_trace,
                    projection: None,
                },
            );
        if replaced.is_some() {
            return Err(format!(
                "duplicate DSV pass evidence for present_seq {present_seq}"
            ));
        }
        CAPTURE_ISSUED.fetch_add(1, Ordering::Relaxed);
    }
    Ok(())
}

/// Caller owns DEPTH_SEND_LOCK; both renderers share evidence and writer accounting.
fn queue_ready_depth_frames(
    ready: Vec<readback::RawDepthFrame>,
    current_present_seq: u64,
    callback_generation: u64,
    tx: &SyncSender<DepthWriterCommand>,
) -> Result<(), String> {
    let requires_observed_projection = if ready.is_empty() {
        false
    } else {
        ACTIVE_CONFIG
            .lock()
            .map_err(|_| "active config lock poisoned during depth drain".to_string())?
            .as_ref()
            .is_some_and(|config| config.camera_production.is_some())
    };
    for frame in ready {
        let evidence = CAPTURE_PASS_EVIDENCE
            .lock()
            .map_err(|_| "DSV pass evidence lock poisoned".to_string())?
            .remove(&frame.present_seq)
            .ok_or_else(|| {
                format!(
                    "readback present_seq {} has no DSV pass evidence",
                    frame.present_seq
                )
            })?;
        let projection = if let Some(projection) = evidence.projection {
            projection
        } else if requires_observed_projection {
            match observed_index_projection(callback_generation, frame.present_seq) {
                Ok(projection) => projection,
                Err(error) => {
                    crate::camera_probe::fail_depth_projection(&error);
                    return Err(error);
                }
            }
        } else {
            active_projection(callback_generation)?.map_or_else(
                depth_lab::capture_timing::IndexProjection::unknown,
                |value| value.index,
            )
        };
        let readback_latency_frames = current_present_seq.saturating_sub(frame.present_seq);
        crate::perf::mark_frame(crate::perf::FrameFlag::Drain);
        if readback_latency_frames >= 2 {
            crate::perf::mark_frame(crate::perf::FrameFlag::Latency2);
        }
        if writer_bypass() {
            WRITER_BYPASSED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let pending = PendingDepthFrame {
            readback_latency_frames,
            frame,
            generation: callback_generation,
            pass: evidence.pass,
            projection,
            identity_trace: evidence.identity_trace.map(Box::new),
        };
        WRITER_QUEUE_DEPTH.fetch_add(1, Ordering::AcqRel);
        match tx.try_send(DepthWriterCommand::Frame(pending)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                WRITER_QUEUE_DEPTH.fetch_sub(1, Ordering::AcqRel);
                WRITER_QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed);
                if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire) {
                    latch_selected_present_fault(
                        depth_lab::selected_present_feedback::FaultReason::WriterQueueFull,
                        "depth writer queue filled while selected-Present mode was active",
                    );
                }
                if !WRITER_QUEUE_ERROR_LOGGED.swap(true, Ordering::Relaxed) {
                    log("P1d writer 队列已满，主动丢弃 depth（渲染线程不阻塞）");
                }
            }
            Err(TrySendError::Disconnected(_)) => {
                WRITER_QUEUE_DEPTH.fetch_sub(1, Ordering::AcqRel);
                return Err("P1d writer 线程已退出".into());
            }
        }
    }
    Ok(())
}

fn capture_max_frames() -> Option<u64> {
    let value = CAPTURE_MAX_FRAMES.load(Ordering::Acquire);
    (value != u64::MAX).then_some(value)
}

fn capture_budget_available(issued: u64, limit: Option<u64>) -> bool {
    limit.is_none_or(|limit| issued < limit)
}

fn snapshot_index(next: usize, len: usize, available: usize, from_end: usize) -> Option<usize> {
    if len == 0 || from_end == 0 || from_end > available.min(len) {
        return None;
    }
    Some((next + len - from_end) % len)
}

fn drain_ready_and_queue(
    swapchain: *mut core::ffi::c_void,
    current_present_seq: u64,
    callback_generation: u64,
) -> Result<(), String> {
    if !readback::has_in_flight() {
        return Ok(());
    }
    let sc =
        unsafe { IDXGISwapChain::from_raw_borrowed(&swapchain) }.ok_or("swapchain 指针无效")?;
    let device: ID3D11Device = unsafe { sc.GetDevice() }.map_err(|e| format!("GetDevice: {e}"))?;
    let ctx: ID3D11DeviceContext = unsafe { device.GetImmediateContext() }
        .map_err(|e| format!("拿不到 immediate context: {e}"))?;
    let tx = DEPTH_TX.get().ok_or("深度 writer 未初始化")?;
    let _send_guard = DEPTH_SEND_LOCK
        .try_lock()
        .map_err(|_| "late depth callback raced with writer stop".to_string())?;
    let generation = ACTIVE_GENERATION.load(Ordering::Acquire);
    if callback_generation == 0 || generation != callback_generation {
        return Err(format!(
            "discarded stale depth callback generation={callback_generation} active={generation}"
        ));
    }

    let ready = readback::drain_ready(&ctx)?;
    if SELECTED_PRESENT_ENABLED.load(Ordering::Acquire)
        && let Some(stats) = readback::stats()
    {
        if stats.drops.timed_out != 0 {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::ReadbackTimeout,
                format!(
                    "{} selected depth readbacks exceeded the deadline",
                    stats.drops.timed_out
                ),
            );
        }
        if stats.drops.pool_full != 0 {
            latch_selected_present_fault(
                depth_lab::selected_present_feedback::FaultReason::ReadbackPoolFull,
                format!(
                    "{} selected depth readbacks found the staging pool full",
                    stats.drops.pool_full
                ),
            );
        }
    }
    queue_ready_depth_frames(ready, current_present_seq, callback_generation, tx)?;
    // deadline 回收或几何变化重建池时不会产出 RawDepthFrame；同步清掉这些帧的
    // pass 证据，避免长 run 在持续 GPU 超时下无界积累。
    let in_flight: BTreeSet<_> = readback::in_flight_present_seqs().into_iter().collect();
    CAPTURE_PASS_EVIDENCE
        .lock()
        .map_err(|_| "DSV pass evidence lock poisoned".to_string())?
        .retain(|present_seq, _| in_flight.contains(present_seq));
    OBSERVED_DRAW_VIEWPORTS
        .lock()
        .map_err(|_| "exact-draw viewport lock poisoned".to_string())?
        .retain(|present_seq, _| in_flight.contains(present_seq));
    Ok(())
}

/// 建一个隐藏窗口 + 临时 swapchain，只为读取 `IDXGISwapChain` 的 vtable 布局。
///
/// 这是 D3D hook 的标准做法：同一进程内所有 `IDXGISwapChain` 实例共享同一张 vtable，
/// 所以在我们自己造的实例上装 hook，对游戏的实例同样生效。
fn create_dummy_swapchain()
-> Result<(IDXGISwapChain, ID3D11Device, ID3D11DeviceContext, HWND), String> {
    unsafe {
        let class = WNDCLASSEXW {
            cbSize: core::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(dummy_wndproc),
            lpszClassName: w!("DmDepthProbeDummy"),
            ..Default::default()
        };
        RegisterClassExW(&class);

        let hwnd = CreateWindowExW(
            Default::default(),
            w!("DmDepthProbeDummy"),
            w!("dm-depth-probe"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            64,
            64,
            None,
            None,
            None,
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;

        let desc = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 64,
                Height: 64,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 1,
            OutputWindow: hwnd,
            Windowed: true.into(),
            ..Default::default()
        };

        let mut swapchain = None;
        let mut device = None;
        let mut context = None;
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&desc),
            Some(&mut swapchain),
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .map_err(|e| format!("D3D11CreateDeviceAndSwapChain: {e}"))?;

        Ok((
            swapchain.ok_or("swapchain 为空")?,
            device.ok_or("device 为空")?,
            context.ok_or("context 为空")?,
            hwnd,
        ))
    }
}

unsafe extern "system" fn dummy_wndproc(
    hwnd: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

// P1b: B0 已枚举 DSV 描述、覆盖全部 raster Draw* 入口计数，并把逐帧观察接入
//      depth_lab::dsv::rank。dump-dsv 图像与人工签字仍属于 #577 L2，不能由 ranking 代替。
// P1c: selected DSV unbind snapshot #4 from frame end → CopyResource → STAGING → Map → ReadbackPool
//      已接通并由 BioShock Infinite 真机灰度图确认包含完整主场景深度。

#[cfg(test)]
mod probe_tests {
    #[test]
    fn presentation_only_stages_never_report_a_player_stream() {
        use super::flushed_player_pose_row_count;
        // KCD2's live-pose summary reports Some(0) in baseline/watermark-only; the client's
        // empty-stage contract requires None there, or the certification fails its first stage.
        assert_eq!(flushed_player_pose_row_count(true, Some(0)), None);
        assert_eq!(flushed_player_pose_row_count(true, None), None);
        // A capture stage keeps the distinction between an empty enabled stream and none.
        assert_eq!(flushed_player_pose_row_count(false, Some(0)), Some(0));
        assert_eq!(
            flushed_player_pose_row_count(false, Some(9_333)),
            Some(9_333)
        );
        assert_eq!(flushed_player_pose_row_count(false, None), None);
    }

    #[test]
    fn presentation_warmup_requires_real_presents_and_stage_specific_watermark() {
        use super::{CaptureWarmup, CaptureWarmupEvidence};
        let start = std::time::Instant::now();
        let observation =
            |adapter, success, required, written| CaptureWarmupEvidence::Presentation {
                adapter_observed: adapter,
                present_succeeded: success,
                watermark_required: required,
                watermark_written: written,
            };
        for watermark_required in [false, true] {
            let mut warmup = CaptureWarmup::new(7);
            for index in 0..=10 {
                assert!(!warmup.observe(
                    7,
                    start + std::time::Duration::from_millis(index * 70),
                    observation(true, false, watermark_required, true)
                ));
            }
            assert_eq!(warmup.qualified_presents, 0);
            assert!(!warmup.observe(6, start, observation(true, true, watermark_required, true)));
            assert!(!warmup.observe(7, start, observation(false, true, watermark_required, true)));
            if watermark_required {
                assert!(!warmup.observe(7, start, observation(true, true, true, false)));
            }
            for index in 0..=8 {
                assert_eq!(
                    warmup.observe(
                        7,
                        start + std::time::Duration::from_millis(1000 + index * 70),
                        observation(true, true, watermark_required, watermark_required)
                    ),
                    index == 8
                );
            }
        }
        // The same missing pose/depth remains fatal to capture warmup.
        let mut full = CaptureWarmup::new(8);
        for index in 0..=20 {
            assert!(!full.observe(
                8,
                start + std::time::Duration::from_millis(index * 70),
                warmup_observation(false)
            ));
        }
        assert_eq!(full.qualified_presents, 0);
    }

    #[test]
    fn only_readback_stages_enable_d3d12_deep_observation() {
        assert!(!super::d3d12_deep_observation_required(true, true));
        assert!(!super::d3d12_deep_observation_required(true, false));
        assert!(!super::d3d12_deep_observation_required(false, true));
        assert!(super::d3d12_deep_observation_required(false, false));
    }

    #[test]
    fn d3d12_hot_path_artifact_subtracts_ready_from_stop() {
        let root =
            std::env::temp_dir().join(format!("dm-d3d12-hot-path-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let start = std::collections::BTreeMap::from([(
            "selected_draw".to_owned(),
            crate::hook_timing::TimingSnapshot {
                calls: 7,
                total_us: 90,
                max_us: 50,
            },
        )]);
        let end = std::collections::BTreeMap::from([
            (
                "selected_draw".to_owned(),
                crate::hook_timing::TimingSnapshot {
                    calls: 10,
                    total_us: 150,
                    max_us: 80,
                },
            ),
            (
                "production_draw_prefilter_rejected".to_owned(),
                crate::hook_timing::TimingSnapshot {
                    calls: 4,
                    ..Default::default()
                },
            ),
        ]);

        super::write_d3d12_hot_path_metrics(&root, 42, &start, &end).unwrap();
        let artifact: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("perf-d3d12-hot-path.json")).unwrap())
                .unwrap();
        assert_eq!(artifact["schema"], "dm-perf-d3d12-hot-path-v1");
        assert_eq!(artifact["generation"], 42);
        assert_eq!(artifact["delta"]["selected_draw"]["calls"], 3);
        assert_eq!(artifact["delta"]["selected_draw"]["total_us"], 60);
        assert_eq!(
            artifact["delta"]["production_draw_prefilter_rejected"]["calls"],
            4
        );
        let overwrite = super::write_d3d12_hot_path_metrics(&root, 42, &start, &end)
            .expect_err("hot-path artifact must be immutable");
        assert!(overwrite.contains("create"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn asynchronous_probe_fault_reaches_client_before_transport_cleanup() {
        const NAME: &str =
            "probe::probe_tests::asynchronous_probe_fault_reaches_client_before_transport_cleanup";
        if std::env::var("DM_PROBE_FAULT_TEST_CHILD").as_deref() != Ok(NAME) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env("DM_PROBE_FAULT_TEST_CHILD", NAME)
                .output()
                .unwrap();
            assert!(
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout)
                        .contains("test result: ok. 1 passed;"),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        use super::*;
        use depth_lab::selected_present_candidate_ring::{CandidateRingConfig, CandidateRingState};
        use depth_lab::selected_present_feedback::{
            FaultReason, FeedbackState, ProtocolError, StreamEpoch, WireIdentity,
        };
        use depth_lab::selected_present_transport::{
            ClientTransport, ProbeTransport, TransportError, TransportNames,
        };
        let stream = StreamEpoch {
            identity: WireIdentity::new("fault-session", "r0000").unwrap(),
            generation: 43,
            resource_epoch: 1,
        };
        let names = TransportNames::new(
            std::process::id(),
            std::process::id(),
            43,
            &uuid::Uuid::new_v4().simple().to_string(),
        )
        .unwrap();
        let client = ClientTransport::create(&names, &stream, 2).unwrap();
        let transport = ProbeTransport::open(&names, &stream).unwrap();
        install_selected_present_runtime(Some(SelectedPresentProbeRuntime {
            transport,
            state: FeedbackState::new(stream.clone()),
            ring_state: CandidateRingState::new(
                CandidateRingConfig {
                    capacity: 4,
                    max_feedback_lag_presents: 3,
                    guard_slots: 1,
                },
                43,
                1,
            )
            .unwrap(),
            stream,
            gpu_ring: None,
            d3d12_gpu_ring: None,
            candidate_evidence: BTreeMap::new(),
            candidate_conv: None,
            readback_slots: 4,
            bootstrap: crate::d3d12_candidate::tests::test_bootstrap(),
            pending_arm: None,
            next_ack_ordinal: 1,
            feedback_latency: FeedbackLatencySamples::new(10_000_000).unwrap(),
        }))
        .unwrap();
        latch_selected_present_fault(
            FaultReason::ReadbackMapFailed,
            "missing projection at Present 33739",
        );
        process_selected_present_feedback(33_745, 42);
        client.check_fault().unwrap();
        process_selected_present_feedback(33_745, 43);
        assert!(matches!(
            client.check_fault(),
            Err(TransportError::Protocol(ProtocolError::Fault(
                FaultReason::ReadbackMapFailed
            )))
        ));
        clear_selected_present_runtime();
        assert!(matches!(
            client.check_fault(),
            Err(TransportError::Protocol(ProtocolError::Fault(
                FaultReason::ReadbackMapFailed
            )))
        ));
    }

    #[test]
    fn d3d12_feedback_uses_the_shared_select_alias_retire_seal_path() {
        use super::*;
        use crate::d3d12_candidate::{D3d12CandidateRing, tests::*};
        use depth_lab::selected_present_candidate_ring::{CandidateRingConfig, CandidateRingState};
        use depth_lab::selected_present_feedback::{
            Decision, FeedbackState, StreamEpoch, WireIdentity,
        };
        use depth_lab::selected_present_transport::{
            ClientTransport, ProbeTransport, TransportNames,
        };
        let (device, queue) = warp_device();
        let bootstrap = test_bootstrap();
        let mut gpu =
            D3d12CandidateRing::new(&device, &queue, &bootstrap, 4, 0, ZConvention::Traditional)
                .unwrap();
        wait_until(|| gpu.ready().unwrap());
        let stream = StreamEpoch {
            identity: WireIdentity::new("warp-feedback", "select-alias").unwrap(),
            generation: 43,
            resource_epoch: 1,
        };
        let nonce = format!("d3d12-{}", current_qpc());
        let names =
            TransportNames::new(std::process::id(), std::process::id(), 43, &nonce).unwrap();
        let _client = ClientTransport::create(&names, &stream, 32).unwrap();
        let transport = ProbeTransport::open(&names, &stream).unwrap();
        let mut state = CandidateRingState::new(
            CandidateRingConfig {
                capacity: 4,
                max_feedback_lag_presents: 3,
                guard_slots: 1,
            },
            43,
            1,
        )
        .unwrap();
        state.arm().unwrap();
        let seq = 930_001;
        let source = filled_snapshot(&device, &queue, &[0.25; 35], 4, seq);
        gpu.materialize(&source, state.materialize(seq).unwrap())
            .unwrap();
        let evidence = PendingCaptureEvidence {
            pass: depth_lab::capture_timing::DsvPassEvidence {
                unbind_ordinal_from_end: 1,
                unbind_total: 1,
                draw_calls: 1,
                selection: depth_lab::DsvSelectionAudit {
                    selected_id: "dsv-r1-g1-e1".into(),
                    ranked_top_id: Some("dsv-r1-g1-e1".into()),
                    score_margin: None,
                    selected_matches_top: true,
                    ranking_confident: true,
                    any_msaa: false,
                    operator_exact_pin: true,
                },
            },
            projection: Some(depth_lab::capture_timing::IndexProjection::unknown()),
            identity_trace: None,
        };
        let mut qpf = 0;
        unsafe { QueryPerformanceFrequency(&mut qpf) }.unwrap();
        let mut runtime = SelectedPresentProbeRuntime {
            transport,
            state: FeedbackState::new(stream.clone()),
            stream,
            ring_state: state,
            gpu_ring: None,
            d3d12_gpu_ring: Some(gpu),
            candidate_evidence: BTreeMap::from([(seq, evidence)]),
            candidate_conv: Some(ZConvention::Traditional),
            readback_slots: 4,
            bootstrap,
            pending_arm: None,
            next_ack_ordinal: 1,
            feedback_latency: FeedbackLatencySamples::new(qpf as u64).unwrap(),
        };
        let now = current_qpc();
        let select = Decision::Select {
            depth_required: true,
            session_frame: 0,
            present_seq: seq,
            duplicate: false,
            source_superseded: 0,
            source_qpc: now - 400,
            callback_start_qpc: now - 300,
            callback_ready_qpc: now - 200,
            decision_qpc: now - 100,
        };
        assert_eq!(
            apply_candidate_decision(&mut runtime, &select, seq + 1, 44)
                .unwrap_err()
                .0,
            depth_lab::selected_present_feedback::FaultReason::GenerationMismatch
        );
        apply_candidate_decision(&mut runtime, &select, seq + 1, 43).unwrap();
        let Decision::Select {
            mut session_frame, ..
        } = select
        else {
            unreachable!()
        };
        session_frame += 1;
        let alias = Decision::Select {
            depth_required: true,
            session_frame,
            present_seq: seq,
            duplicate: true,
            source_superseded: 0,
            source_qpc: now - 400,
            callback_start_qpc: now - 300,
            callback_ready_qpc: now - 200,
            decision_qpc: now - 100,
        };
        apply_candidate_decision(&mut runtime, &alias, seq + 2, 43).unwrap();
        apply_candidate_decision(
            &mut runtime,
            &Decision::RetireThrough { present_seq: seq },
            seq + 2,
            43,
        )
        .unwrap();
        apply_candidate_decision(
            &mut runtime,
            &Decision::SealGeneration {
                final_session_frame: Some(1),
                expected_select_count: 2,
                expected_distinct_depth_count: 1,
            },
            seq + 2,
            43,
        )
        .unwrap();
        let gpu = runtime.d3d12_gpu_ring.as_mut().unwrap();
        let mut frames = Vec::new();
        wait_until(|| {
            frames.extend(gpu.drain_ready().unwrap());
            !frames.is_empty()
        });
        assert_eq!(frames.len(), 1);
        assert_eq!(gpu.completed(), runtime.ring_state.stats().submitted);
        assert_eq!(runtime.ring_state.stats().source_identity_aliases, 1);
        assert_eq!(runtime.ring_state.unresolved(), 0);
        assert!(runtime.candidate_evidence.is_empty());
        assert!(CAPTURE_PASS_EVIDENCE.lock().unwrap().remove(&seq).is_some());
    }

    #[test]
    fn compiled_runtime_version_matches_the_release_environment() {
        match std::env::var("DM_DEPTH_PROBE_RUNTIME_VERSION") {
            Ok(expected) => assert_eq!(super::compiled_probe_runtime_version(), expected),
            Err(_) => assert_eq!(
                super::compiled_probe_runtime_version(),
                env!("CARGO_PKG_VERSION")
            ),
        }
    }

    #[test]
    fn missing_early_ready_event_uses_hresult_encoded_win32_code() {
        let file_not_found =
            windows::core::HRESULT::from_win32(windows::Win32::Foundation::ERROR_FILE_NOT_FOUND.0);

        assert_eq!(file_not_found.0 as u32, 0x8007_0002);
        assert!(super::is_missing_early_ready_event(file_not_found));
        assert!(!super::is_missing_early_ready_event(
            windows::core::HRESULT::from_win32(5)
        ));
    }

    #[test]
    fn camera_failure_code_preserves_player_pose_as_the_primary_fault() {
        assert_eq!(
            super::camera_production_fault_code(
                "player pose sample 50622 failed: Fallout actor-root transform was unstable"
            ),
            "player_pose_sampling_failed"
        );
        assert_eq!(
            super::camera_production_fault_code("camera pose sample 9 failed"),
            "camera_pose_sampling_failed"
        );
    }

    use std::sync::Mutex;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::sync_channel;

    use depth_lab::capture_timing::{DsvPassEvidence, IndexProjection};
    use depth_lab::decode::{DepthFormat, ZConvention};
    use depth_lab::{
        CAMERA_DISCOVERY_SCHEMA_V1, CAMERA_DISCOVERY_SCHEMA_V3, CameraDiscoveryConfig,
        CameraDiscoveryDsvSelection, CameraDiscoveryRenderIdentity, DiscoveryDepthFormat,
        ProbeConfig,
    };
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_CONSTANT_BUFFER, D3D11_BIND_DEPTH_STENCIL, D3D11_CREATE_DEVICE_FLAG,
        D3D11_DSV_DIMENSION_TEXTURE2D, D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
        D3D11_DSV_DIMENSION_TEXTURE2DMS, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC,
        D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
        ID3D11DeviceContext,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32G8X24_TYPELESS,
        DXGI_SAMPLE_DESC,
    };
    use windows::core::{BOOL, Interface};

    use super::{
        BackingHookSnapshot, CAPTURE_WARMUP_MAX_PRESENT_GAP, CameraDrawEntryRoute,
        CameraDrawMethod, CameraHookEntryCounters, CaptureWarmup, CaptureWarmupEvidence,
        CaptureWarmupObservation, DEFERRED_HOOK_COVERAGE_FAILURE, DeferredDsvEvent,
        DeferredDsvLedger, DepthSelectionExpectation, DepthSelectionState, DepthWriterCommand,
        DsvCommandListRoute, DsvDrawReacquireRoute, DsvFrameTracker, DsvPairIdentity,
        ExactSnapshotRecord, FeedbackLatencySamples, MAX_DSV_DEFERRED_EVENTS_PER_RECORDING,
        MAX_FEEDBACK_LATENCY_SAMPLES, MAX_RETIRED_DSV_PAIRS, ORIGINAL_DRAW_AUTO,
        ORIGINAL_DRAW_INDEXED_INSTANCED, ORIGINAL_DRAW_INDEXED_INSTANCED_INDIRECT,
        ORIGINAL_DRAW_INSTANCED, ORIGINAL_DRAW_INSTANCED_INDIRECT, ORIGINAL_EXECUTE_COMMAND_LIST,
        ORIGINAL_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS, ObservedDrawViewport,
        ObservedDsvResource, OmBindMethod, OmDepthBindingAction, PASS_IDENTITY_TRACE_FILE,
        PASS_IDENTITY_TRACE_SCHEMA_V1, PassIdentityJoin, PassIdentityTrace, PeerLifecyclePhase,
        PendingDepthFrame, PhaseDepthRecovery, ResolvedProjection, RetainedUnboundTransition,
        RunProjectionState, SelectedDepthLockFailure, SnapshotPassIdentityTrace,
        WARMUP_BLOCKER_PLAYER_SAMPLER, WRITER_QUEUE_DEPTH, accept_reacquired_depth_resource,
        accept_selected_depth_resource, active_fault_command_id, apply_viewport_to_projection,
        backing_draws_restored_inside_present, camera_capture_requires_shadow,
        camera_deep_hook_gate, camera_discovery_phase_reset_config, camera_draw_entry_route,
        camera_draw_gate_with_reacquire, capture_budget_available, capture_warmup_timed_out,
        classify_pass_identity_join, classify_peer_wait, context_shadow_vtable_shape,
        depth_candidate_capture_requested, depth_evidence_gate, depth_format_matches,
        depth_observation_requires_shadow, depth_writer_loop, dsv_command_list_route,
        dsv_discovery_frame_limit_reached, dsv_draw_reacquire_route,
        exact_production_snapshot_mode, exact_recovery_retained_owner_matches,
        exact_snapshot_copy_decision, executable_basename_matches,
        finalize_d3d12_discovery_snapshot, formal_capture_control_enabled, format_driver_version,
        hooked_draw_auto, hooked_draw_indexed_instanced, hooked_draw_indexed_instanced_indirect,
        hooked_draw_instanced, hooked_draw_instanced_indirect, hooked_execute_command_list,
        hooked_om_set_render_targets_and_unordered_access_views, inspect_constant_buffer_if,
        inspect_deferred_dsv, is_selected_depth_desc, latch_exact_selection_ambiguity_state,
        load_depth_profile, map_selected_depth_result, multithread_protection_changed,
        normalize_production_vs_range, om_uav_depth_binding_action, peer_liveness_fault,
        phase_reset_depth_recovery, projection_clip_planes, projection_coefficient_matches,
        projection_family_matches, retained_unbound_transition, select_exact_snapshot,
        selected_depth_lock_failure_is_terminal, sha256_stream, shader_readable_snapshot_format,
        should_publish_first_present, snapshot_index, validate_camera_discovery_executable_binding,
        validate_d3d11_copy_resource_envelope, validate_observed_viewport,
        verify_context_inherited_aliases,
    };
    use crate::camera_probe::{ExactProductionDrawIdentity, MainPassIdentity};
    use crate::readback::RawDepthFrame;
    use crate::vtable::d3d11_context;

    type IndexedInstancedCall = (usize, u32, u32, u32, i32, u32);
    type InstancedCall = (usize, u32, u32, u32, u32);
    type OmUavCall = (usize, u32, usize, usize, u32, u32, usize, usize);

    static ABI_FORWARD_LOCK: Mutex<()> = Mutex::new(());
    static INDEXED_INSTANCED_CALL: Mutex<Option<IndexedInstancedCall>> = Mutex::new(None);
    static INSTANCED_CALL: Mutex<Option<InstancedCall>> = Mutex::new(None);
    static AUTO_CALL: Mutex<Option<usize>> = Mutex::new(None);
    static INDEXED_INDIRECT_CALL: Mutex<Option<(usize, usize, u32)>> = Mutex::new(None);
    static INDIRECT_CALL: Mutex<Option<(usize, usize, u32)>> = Mutex::new(None);
    static EXECUTE_COMMAND_LIST_CALL: Mutex<Option<(usize, usize, bool)>> = Mutex::new(None);
    static OM_UAV_CALL: Mutex<Option<OmUavCall>> = Mutex::new(None);

    fn warp_device_with_deferred_context() -> (ID3D11Device, ID3D11DeviceContext) {
        let mut device = None;
        let mut immediate = None;
        let mut level = Default::default();
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut immediate),
            )
        }
        .expect("WARP device creation failed");
        let device = device.expect("WARP device missing");
        let mut deferred = None;
        unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }
            .expect("WARP deferred context creation failed");
        (device, deferred.expect("WARP deferred context missing"))
    }

    #[test]
    fn warp_context1_through_context4_inherited_slots_share_the_hooked_identity() {
        let (device, deferred) = warp_device_with_deferred_context();
        let immediate = unsafe { device.GetImmediateContext() }.unwrap();
        assert!(
            verify_context_inherited_aliases(&immediate, "WARP immediate test").unwrap(),
            "WARP must expose Context1 for derived-slot coverage"
        );
        assert!(
            verify_context_inherited_aliases(&deferred, "WARP deferred test").unwrap(),
            "WARP must expose Context1 for derived-slot coverage"
        );
    }

    #[test]
    fn multithread_protection_toggle_is_detected_without_false_positives() {
        assert!(!multithread_protection_changed(false, false));
        assert!(!multithread_protection_changed(true, true));
        assert!(multithread_protection_changed(false, true));
        assert!(multithread_protection_changed(true, false));
    }

    #[test]
    fn discovery_only_process_never_starts_formal_or_legacy_capture_control() {
        assert!(!formal_capture_control_enabled(true));
        assert!(formal_capture_control_enabled(false));
    }

    #[test]
    fn empty_d3d12_discovery_snapshot_is_explicitly_insufficient() {
        let snapshot = finalize_d3d12_discovery_snapshot(
            crate::d3d12_backend::D3d12ObservationBackend::new().snapshot(),
        );
        assert!(!snapshot.capture_safe);
        assert_eq!(snapshot.blockers.len(), 3);
        assert!(
            snapshot
                .blockers
                .iter()
                .any(|value| value.contains("swapchain"))
        );
        assert!(
            snapshot
                .blockers
                .iter()
                .any(|value| value.contains("Present"))
        );
        assert!(
            snapshot
                .blockers
                .iter()
                .any(|value| value.contains("depth candidate"))
        );
    }

    #[test]
    fn raw_d32_drain_is_the_only_route_that_bypasses_formal_projection() {
        assert!(super::d3d12_raw_drain_bypasses_formal_projection(true));
        assert!(!super::d3d12_raw_drain_bypasses_formal_projection(false));
    }

    #[test]
    fn formal_present_rate_gate_is_backend_neutral() {
        assert!(super::should_observe_formal_present_rate(true, true));
        assert!(!super::should_observe_formal_present_rate(true, false));
        assert!(!super::should_observe_formal_present_rate(false, true));
    }

    #[test]
    fn terminal_fault_uses_stop_command_once_stop_was_accepted() {
        assert_eq!(active_fault_command_id("start-1", None), "start-1");
        assert_eq!(
            active_fault_command_id("start-1", Some("stop-before-seal")),
            "stop-before-seal"
        );
        assert_eq!(
            active_fault_command_id("start-1", Some("stop-after-stopped")),
            "stop-after-stopped"
        );
    }

    #[test]
    fn peer_exit_is_terminal_before_and_after_generation_seal() {
        use windows::Win32::Foundation::{WAIT_EVENT, WAIT_OBJECT_0, WAIT_TIMEOUT};

        assert_eq!(classify_peer_wait(WAIT_OBJECT_0), Ok(true));
        assert_eq!(classify_peer_wait(WAIT_TIMEOUT), Ok(false));
        assert!(classify_peer_wait(WAIT_EVENT(7)).is_err());
        for phase in [
            PeerLifecyclePhase::Starting,
            PeerLifecyclePhase::Recording,
            PeerLifecyclePhase::Sealed,
            PeerLifecyclePhase::Stopping,
        ] {
            let fault = peer_liveness_fault(42, phase, Ok(true));
            assert!(fault.is_some(), "peer exit was ignored during {phase:?}");
            assert_eq!(peer_liveness_fault(42, phase, Ok(false)), None);
        }
    }

    #[test]
    fn local_compatibility_requires_this_run_evidence_and_preserves_legacy_mode() {
        const NAME: &str = "probe::probe_tests::local_compatibility_requires_this_run_evidence_and_preserves_legacy_mode";
        const CHILD: &str = "DM_LOCAL_COMPATIBILITY_TEST_CHILD";
        if std::env::var(CHILD).as_deref() != Ok(NAME) {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--nocapture"])
                .env(CHILD, NAME)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let identity = depth_lab::StreamIdentity {
            session_id: "test".into(),
            run_id: "r0".into(),
            generation: 7,
        };
        let temp =
            std::env::temp_dir().join(format!("dm-compatibility-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();
        super::configure_local_compatibility(&identity, Some(30)).unwrap();
        assert!(!super::LOCAL_COMPAT_PASSED.load(Ordering::Acquire));
        assert!(super::persist_local_compatibility(&identity, &temp).is_err());
        let mut gate = depth_lab::compatibility::CadenceCheck::new(30, 3000).unwrap();
        for i in 0..=60 {
            gate.observe(1 + i * 100);
        }
        *super::LOCAL_COMPAT_CHECK.lock().unwrap() = Some((identity.clone(), gate));
        super::persist_local_compatibility(&identity, &temp).unwrap();
        let receipt: depth_lab::compatibility::LocalCompatibilityReceipt = serde_json::from_slice(
            &std::fs::read(temp.join(depth_lab::compatibility::RECEIPT_FILE)).unwrap(),
        )
        .unwrap();
        receipt.validate(&identity, 30).unwrap();
        let mut stale = identity.clone();
        stale.generation = 8;
        assert!(super::persist_local_compatibility(&stale, &temp).is_err());
        assert!(super::local_compatibility_error(&stale).is_some());
        assert!(super::persist_local_compatibility(&identity, &temp).is_err());
        super::configure_local_compatibility(&identity, None).unwrap();
        assert!(super::local_compatibility_error(&identity).is_none());
        let empty = temp.join("empty");
        std::fs::create_dir(&empty).unwrap();
        super::persist_local_compatibility(&identity, &empty).unwrap();
        assert!(!empty.join(depth_lab::compatibility::RECEIPT_FILE).exists());
        std::fs::remove_dir_all(&temp).unwrap();
    }

    fn warmup_observation(ready: bool) -> CaptureWarmupObservation {
        CaptureWarmupObservation {
            hooks_installed: ready,
            hook_integrity_healthy: ready,
            selected_depth_ready: ready,
            dsv_deliverable: ready,
            exact_draw_viewport_ready: ready,
            camera: crate::camera_probe::WarmupCameraReadiness {
                production_ready: ready,
                view_ready: ready,
                projection_ready: ready,
                camera_sample_error: None,
                player_sample_error: None,
            },
        }
    }

    #[test]
    fn warmup_summary_attributes_blocked_presents_without_changing_readiness() {
        let start = std::time::Instant::now();
        let mut warmup = CaptureWarmup::new(5);
        // 前 20 帧只差 CP2077 的位姿对帧：其余判据全通过。
        for present in 1..=20_u64 {
            let mut observation = warmup_observation(true);
            observation.camera.player_sample_error =
                Some("no sampled CP2077 simulation state carries this Present's camera".into());
            let now = start + std::time::Duration::from_millis(present * 50);
            assert!(!warmup.observe(5, now, observation), "缺位姿对帧不得判就绪");
        }
        // 随后恢复：8 帧连续 + 跨度 ≥500 ms 才放行，判定式一个字没动。
        let mut ready_at_present = None;
        for present in 21..=40_u64 {
            let now = start + std::time::Duration::from_millis(present * 50);
            if warmup.observe(5, now, warmup_observation(true)) {
                ready_at_present = Some(present);
                break;
            }
        }
        // 第 21 帧起算，第 8 帧（present 28）满足连续数，但跨度只有 350 ms；
        // 要到 present 31 才够 500 ms。这正是改动前的行为。
        assert_eq!(ready_at_present, Some(31));

        let blocked = warmup.blocker_presents[WARMUP_BLOCKER_PLAYER_SAMPLER];
        assert_eq!(blocked, 20, "位姿对帧应当被记成卡住 20 帧");
        assert!(
            warmup
                .blocker_presents
                .iter()
                .enumerate()
                .all(|(index, count)| index == WARMUP_BLOCKER_PLAYER_SAMPLER || *count == 0),
            "其余判据没有卡住任何一帧，不该被记账"
        );

        let summary = warmup.completion_summary_json();
        assert!(summary.contains("\"player_sampler\":20"), "{summary}");
        assert!(
            !summary.contains("camera_production_not_ready"),
            "{summary}"
        );
        assert!(summary.contains("\"continuity_resets\":0"), "{summary}");
        assert!(summary.contains("\"qualified_presents\":11"), "{summary}");
        // 三个时间戳都必须落到摘要里，否则真机上分不清「等第一帧」和「等判据通过」。
        for key in ["first_present_ms", "first_qualified_ms", "ready_ms"] {
            assert!(
                summary.contains(key) && !summary.contains(&format!("\"{key}\":null")),
                "{summary}"
            );
        }
    }

    #[test]
    fn warmup_blocker_flags_and_text_stay_one_definition() {
        let mut observation = warmup_observation(true);
        observation.camera.view_ready = false;
        observation.camera.projection_ready = false;
        let evidence = CaptureWarmupEvidence::from(observation);
        // if / else if 链：只报最靠前的那一条，不能两条一起报。
        assert_eq!(
            evidence.blockers(),
            vec!["exact_draw_view_missing_or_invalid"]
        );
        assert_eq!(
            evidence.blocker_flags().count_ones(),
            evidence.blockers().len() as u32,
            "位图与文案必须逐条对应"
        );

        let mut observation = warmup_observation(false);
        observation.camera.camera_sample_error = Some("lock is contended".into());
        let evidence = CaptureWarmupEvidence::from(observation);
        let blockers = evidence.blockers();
        // 采样器那两条要带上原始错误，其余是纯名字。
        assert!(blockers.contains(&"camera_sampler=lock is contended".to_string()));
        assert!(blockers.contains(&"game_context_hooks_not_installed".to_string()));
        assert_eq!(evidence.blocker_flags().count_ones(), blockers.len() as u32);
    }

    #[test]
    fn selected_present_seal_never_falls_back_to_legacy_cadence_capture() {
        assert!(depth_candidate_capture_requested(true, false, false, false));
        assert!(!depth_candidate_capture_requested(
            false, false, false, false
        ));
        assert!(!depth_candidate_capture_requested(
            false, true, false, false
        ));
        assert!(!depth_candidate_capture_requested(true, true, false, false));
        assert!(depth_candidate_capture_requested(false, true, true, false));
        assert!(depth_candidate_capture_requested(true, true, true, false));
        assert!(!depth_candidate_capture_requested(false, true, true, true));
        assert!(!depth_candidate_capture_requested(true, true, true, true));
    }

    #[test]
    fn first_present_baseline_is_published_only_after_visible_success() {
        assert!(should_publish_first_present(true, true, false, 0));
        assert!(should_publish_first_present(true, true, false, 1));
        assert!(should_publish_first_present(true, false, true, 0));
        assert!(!should_publish_first_present(false, true, false, 0));
        assert!(!should_publish_first_present(true, false, false, 0));
        assert!(!should_publish_first_present(true, true, false, -1));
    }

    #[test]
    fn swapchain_driver_version_uses_the_same_dxgi_format_as_preflight() {
        use windows::core::Interface;

        assert_eq!(
            super::dxgi_driver_version_query_iid(),
            windows::Win32::Graphics::Dxgi::IDXGIDevice::IID
        );
        assert_ne!(
            super::dxgi_driver_version_query_iid(),
            windows::Win32::Graphics::Direct3D12::ID3D12Device::IID
        );
        let raw = ((32u64 << 48) | (15u64 << 16) | 9_227) as i64;
        assert_eq!(format_driver_version(raw), "32.0.15.9227");
    }

    #[test]
    fn production_vs_range_normalizes_only_the_legacy_whole_buffer_sentinel() {
        assert_eq!(
            normalize_production_vs_range(752, 752, Some((0, 65_536))),
            Ok((0, 752))
        );
        assert_eq!(
            normalize_production_vs_range(752, 1_024, Some((16, 752))),
            Ok((16, 752))
        );
        assert_eq!(normalize_production_vs_range(752, 752, None), Ok((0, 752)));

        for invalid in [
            normalize_production_vs_range(752, 752, Some((16, 65_536))),
            normalize_production_vs_range(752, 1_024, Some((0, 65_536))),
            normalize_production_vs_range(752, 752, Some((0, 32_768))),
            normalize_production_vs_range(752, 1_024, None),
        ] {
            assert!(invalid.is_err());
        }
    }

    /// What each reading is allowed to be lives in the contract crate, with
    /// the ordering and ceiling table
    /// (`present_identity::feedback::timestamp_tests`). This test owns the
    /// other half: that an accepted sample lands in the right bucket and that
    /// the summary reconciles.
    #[test]
    fn feedback_latency_sorts_each_sample_into_exactly_one_bucket() {
        let mut samples = FeedbackLatencySamples::new(10_000_000).unwrap();
        for index in 0..100u64 {
            let source = 1_000_000 + index as i64 * 100_000;
            samples
                .observe(
                    1_000 + index,
                    1_010 + index,
                    depth_lab::selected_present_feedback::SelectTimestamps {
                        source,
                        callback_start: source - 2_000,
                        callback_ready: source + 10_000,
                        decision: source + 30_000,
                        process: source + 40_000 + index as i64 * 100,
                    },
                )
                .unwrap();
        }
        samples
            .observe(
                2_000,
                2_010,
                depth_lab::selected_present_feedback::SelectTimestamps {
                    source: 14_795,
                    callback_start: 10_000,
                    callback_ready: 10_100,
                    decision: 20_000,
                    process: 30_000,
                },
            )
            .unwrap();
        // Verbatim from the Fallout 4 / RTX 5060 Ti run that this fix came
        // from: the compositor clock leads even the probe read, by 105us.
        samples
            .observe(
                3_000,
                3_004,
                depth_lab::selected_present_feedback::SelectTimestamps {
                    source: 2_405_971_127_436,
                    callback_start: 2_405_971_020_694,
                    callback_ready: 2_405_971_020_766,
                    decision: 2_405_971_109_578,
                    process: 2_405_971_126_386,
                },
            )
            .unwrap();
        let summary = samples.summary();
        assert_eq!(summary.sample_count, 102);
        assert_eq!(summary.selected_lag_presents.p99, 10);
        assert_eq!(summary.wgc_delivery_sample_count, 100);
        assert_eq!(summary.wgc_delivery_us.p50, 1_000);
        assert_eq!(summary.wgc_source_ahead_callback_ready_count, 2);
        assert_eq!(summary.wgc_source_ahead_callback_ready_us.max, 10_667);
        assert_eq!(summary.callback_to_decision_us.p95, 2_000);
        assert!(summary.source_to_probe_us.p99 >= summary.source_to_probe_us.p95);
        assert_eq!(summary.source_to_probe_sample_count, 101);
        assert_eq!(summary.wgc_source_ahead_process_count, 1);
        assert_eq!(summary.wgc_source_ahead_process_us.max, 105);
        // Both splits cover every decision, which is what FlushSummary checks
        // before the run may back a performance certification.
        assert_eq!(
            summary.wgc_delivery_sample_count + summary.wgc_source_ahead_callback_ready_count,
            summary.sample_count
        );
        assert_eq!(
            summary.source_to_probe_sample_count + summary.wgc_source_ahead_process_count,
            summary.sample_count
        );
    }

    #[test]
    fn feedback_latency_storage_is_hard_bounded() {
        let mut samples = FeedbackLatencySamples::new(10_000_000).unwrap();
        samples.selected_lag_presents = vec![0; MAX_FEEDBACK_LATENCY_SAMPLES];
        let error = samples
            .observe(
                1,
                1,
                depth_lab::selected_present_feedback::SelectTimestamps {
                    source: 10,
                    callback_start: 20,
                    callback_ready: 25,
                    decision: 30,
                    process: 40,
                },
            )
            .unwrap_err();
        assert_eq!(
            error.0,
            depth_lab::selected_present_feedback::FaultReason::FeedbackTelemetryOverflow
        );
    }

    #[test]
    fn formal_non_capture_frames_skip_deep_render_evidence_but_warmup_does_not() {
        assert!(depth_evidence_gate(true, false, false, false, true));
        assert!(
            depth_evidence_gate(true, true, true, false, true),
            "discovery must keep observing independently of formal cadence"
        );
        assert!(depth_evidence_gate(true, false, true, true, true));
        assert!(!depth_evidence_gate(true, false, true, false, true));
        assert!(!depth_evidence_gate(true, false, true, true, false));
        assert!(!depth_evidence_gate(false, true, false, true, true));
    }

    #[test]
    fn non_sampled_map_and_update_paths_do_not_run_com_inspection() {
        let inspections = std::cell::Cell::new(0);
        let result = inspect_constant_buffer_if(0, true, true, false, || {
            inspections.set(inspections.get() + 1);
            Some(128usize)
        });
        assert_eq!(result, None);
        assert_eq!(inspections.get(), 0);

        let result = inspect_constant_buffer_if(0, true, true, true, || {
            inspections.set(inspections.get() + 1);
            Some(128usize)
        });
        assert_eq!(result, Some(128));
        assert_eq!(inspections.get(), 1);
    }

    #[test]
    fn exact_projection_closes_camera_deep_hooks_but_not_discovery() {
        assert!(camera_deep_hook_gate(true, false, false));
        assert!(!camera_deep_hook_gate(true, false, true));
        assert!(camera_deep_hook_gate(true, true, true));
        assert!(!camera_deep_hook_gate(false, false, false));
    }

    #[test]
    fn observed_projection_family_allows_dynamic_far_but_rejects_near_or_z_drift() {
        let expected = 15.000636100769045;
        assert!(projection_coefficient_matches(
            expected,
            f64::from(expected as f32)
        ));
        assert!(!projection_coefficient_matches(expected, expected + 0.01));

        let profile = depth_lab::Projection {
            a: 1.000_042_438_507_08,
            b: -15.000_636_100_769_045,
            metric_scale: 0.014_287_5,
        };
        let interior = depth_lab::Projection {
            a: 1.001_502_275_466_919,
            b: -15.022_533_416_748_047,
            metric_scale: 0.014_287_5,
        };
        assert!(projection_family_matches(profile, interior));
        let (convention, near, far) = projection_clip_planes(interior).unwrap();
        assert_eq!(convention, ZConvention::Traditional);
        assert!((near - 15.0).abs() < 1.0e-6);
        assert!((9_999.0..=10_001.0).contains(&far.unwrap()));

        let wrong_near = depth_lab::Projection::traditional_z(16.0, 10_000.0, 0.014_287_5);
        let wrong_z = depth_lab::Projection::reversed_z_infinite(15.0, 0.014_287_5);
        let malformed_far = depth_lab::Projection {
            a: 0.5,
            b: -7.5,
            metric_scale: 0.014_287_5,
        };
        assert!(!projection_family_matches(profile, wrong_near));
        assert!(!projection_family_matches(profile, wrong_z));
        assert!(!projection_family_matches(profile, malformed_far));
    }

    #[test]
    fn viewport_mapping_is_applied_to_raw_depth_coefficients() {
        let effective = apply_viewport_to_projection(
            depth_lab::Projection {
                a: 0.25,
                b: 2.0,
                metric_scale: 0.01,
            },
            0.1,
            0.9,
        );
        assert!((effective.a - 0.3).abs() < f64::EPSILON);
        assert!((effective.b - 1.6).abs() < f64::EPSILON);
        assert_eq!(effective.metric_scale, 0.01);
    }

    #[test]
    fn exact_draw_viewport_validation_fails_closed() {
        let valid = ObservedDrawViewport {
            generation: 7,
            count: 1,
            min_depth: 0.0098,
            max_depth: 1.0,
        };
        assert_eq!(validate_observed_viewport(valid), Ok((0.0098, 1.0)));
        assert!(validate_observed_viewport(ObservedDrawViewport { count: 2, ..valid }).is_err());
        assert!(
            validate_observed_viewport(ObservedDrawViewport {
                max_depth: 0.0098,
                ..valid
            })
            .is_err()
        );
        assert!(
            validate_observed_viewport(ObservedDrawViewport {
                min_depth: f64::NAN,
                ..valid
            })
            .is_err()
        );
    }

    #[test]
    fn projection_state_is_replaced_only_after_the_previous_generation_stops() {
        let projection = |metric_scale, readback_slots| ResolvedProjection {
            conv: ZConvention::Traditional,
            index: IndexProjection::observed(
                depth_lab::Projection {
                    a: 1.001,
                    b: -0.1,
                    metric_scale,
                },
                Some(metric_scale),
            ),
            readback_slots,
        };
        let mut state = RunProjectionState::new();

        state.start(1, Some(projection(0.01, 6)), false).unwrap();
        assert_eq!(
            state.get(1).unwrap().unwrap().index.metric_scale,
            Some(0.01)
        );
        assert_eq!(state.get(1).unwrap().unwrap().readback_slots, 6);
        assert!(state.start(2, Some(projection(0.02, 8)), false).is_err());

        state.stop(1).unwrap();
        state.start(2, Some(projection(0.02, 8)), false).unwrap();
        assert!(state.get(1).is_err(), "stale generations must fail closed");
        assert_eq!(
            state.get(2).unwrap().unwrap().index.metric_scale,
            Some(0.02),
            "the next StartRun must use its own profile evidence"
        );
        assert_eq!(
            state.get(2).unwrap().unwrap().readback_slots,
            8,
            "the next StartRun must use its own readback pool size"
        );
    }

    #[test]
    fn shipped_profile_readback_slots_reach_runtime_projection() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../config/catalogs/dev/fallout4-steam-release-test/depth-profile.json");

        let resolved = load_depth_profile(&path, None).unwrap();

        assert_eq!(resolved.readback_slots, 6);
    }

    #[test]
    fn unvalidated_projection_family_is_accepted_only_for_validation_runs() {
        let mut state = RunProjectionState::new();

        state.start(1, None, false).unwrap();
        assert_eq!(
            state.accept_unvalidated_projection_family(1).unwrap(),
            None,
            "formal capture must fail closed"
        );
        state.stop(1).unwrap();

        state.start(2, None, true).unwrap();
        assert_eq!(
            state.accept_unvalidated_projection_family(2).unwrap(),
            Some(true),
            "the first accepted observation should be logged"
        );
        assert_eq!(
            state.accept_unvalidated_projection_family(2).unwrap(),
            Some(false),
            "later observations should not spam the probe log"
        );
    }

    unsafe extern "system" fn fake_draw_indexed_instanced(
        context: *mut core::ffi::c_void,
        index_count_per_instance: u32,
        instance_count: u32,
        start_index_location: u32,
        base_vertex_location: i32,
        start_instance_location: u32,
    ) {
        if let Ok(mut call) = INDEXED_INSTANCED_CALL.lock() {
            *call = Some((
                context as usize,
                index_count_per_instance,
                instance_count,
                start_index_location,
                base_vertex_location,
                start_instance_location,
            ));
        }
    }

    unsafe extern "system" fn fake_draw_instanced(
        context: *mut core::ffi::c_void,
        vertex_count_per_instance: u32,
        instance_count: u32,
        start_vertex_location: u32,
        start_instance_location: u32,
    ) {
        if let Ok(mut call) = INSTANCED_CALL.lock() {
            *call = Some((
                context as usize,
                vertex_count_per_instance,
                instance_count,
                start_vertex_location,
                start_instance_location,
            ));
        }
    }

    unsafe extern "system" fn fake_draw_auto(context: *mut core::ffi::c_void) {
        if let Ok(mut call) = AUTO_CALL.lock() {
            *call = Some(context as usize);
        }
    }

    unsafe extern "system" fn fake_draw_indexed_indirect(
        context: *mut core::ffi::c_void,
        buffer: *mut core::ffi::c_void,
        offset: u32,
    ) {
        if let Ok(mut call) = INDEXED_INDIRECT_CALL.lock() {
            *call = Some((context as usize, buffer as usize, offset));
        }
    }

    unsafe extern "system" fn fake_draw_indirect(
        context: *mut core::ffi::c_void,
        buffer: *mut core::ffi::c_void,
        offset: u32,
    ) {
        if let Ok(mut call) = INDIRECT_CALL.lock() {
            *call = Some((context as usize, buffer as usize, offset));
        }
    }

    unsafe extern "system" fn fake_execute_command_list(
        context: *mut core::ffi::c_void,
        command_list: *mut core::ffi::c_void,
        restore_context_state: BOOL,
    ) {
        if let Ok(mut call) = EXECUTE_COMMAND_LIST_CALL.lock() {
            *call = Some((
                context as usize,
                command_list as usize,
                restore_context_state.as_bool(),
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    unsafe extern "system" fn fake_om_uav(
        context: *mut core::ffi::c_void,
        num_views: u32,
        render_target_views: *const *mut core::ffi::c_void,
        depth_stencil_view: *mut core::ffi::c_void,
        uav_start_slot: u32,
        num_uavs: u32,
        unordered_access_views: *const *mut core::ffi::c_void,
        uav_initial_counts: *const u32,
    ) {
        if let Ok(mut call) = OM_UAV_CALL.lock() {
            *call = Some((
                context as usize,
                num_views,
                render_target_views as usize,
                depth_stencil_view as usize,
                uav_start_slot,
                num_uavs,
                unordered_access_views as usize,
                uav_initial_counts as usize,
            ));
        }
    }

    #[test]
    fn snapshot_ring_selects_from_the_newest_copy() {
        assert_eq!(snapshot_index(4, 12, 12, 1), Some(3));
        assert_eq!(snapshot_index(4, 12, 12, 4), Some(0));
        assert_eq!(snapshot_index(1, 12, 12, 4), Some(9));
    }

    #[test]
    fn snapshot_ring_rejects_unavailable_history() {
        assert_eq!(snapshot_index(2, 12, 2, 3), None);
        assert_eq!(snapshot_index(2, 12, 2, 0), None);
    }

    #[test]
    fn pass_identity_join_never_turns_an_ordinal_guess_into_an_exact_match() {
        let candidate = |from_end, pass, projection_observed| SnapshotPassIdentityTrace {
            unbind_ordinal_from_start: 7 - from_end,
            unbind_ordinal_from_end: from_end,
            pass_ordinal: Some(pass),
            draw_calls: 100,
            exact_draw_ordinal: (pass == 0).then_some(6),
            projection_observed,
        };
        let exact = ExactProductionDrawIdentity {
            pass_ordinal: 0,
            draw_ordinal: 6,
            projection_observed: true,
        };
        let candidates = vec![candidate(1, 5, false), candidate(5, 0, true)];

        assert_eq!(
            classify_pass_identity_join(Some(exact), 5, &candidates),
            PassIdentityJoin::Exact
        );
        assert_eq!(
            classify_pass_identity_join(Some(exact), 1, &candidates),
            PassIdentityJoin::SelectedSnapshotDiffers
        );
        assert_eq!(
            classify_pass_identity_join(Some(exact), 5, &candidates[..1]),
            PassIdentityJoin::ProjectionPassNotSnapshotted
        );
        assert_eq!(
            classify_pass_identity_join(
                Some(ExactProductionDrawIdentity {
                    projection_observed: false,
                    ..exact
                }),
                5,
                &candidates,
            ),
            PassIdentityJoin::ProjectionObservationMissing
        );
        assert_eq!(
            classify_pass_identity_join(None, 5, &candidates),
            PassIdentityJoin::ExactDrawMissing
        );
        let duplicate = vec![candidate(5, 0, true), candidate(4, 0, true)];
        assert_eq!(
            classify_pass_identity_join(Some(exact), 5, &duplicate),
            PassIdentityJoin::Ambiguous
        );
    }

    #[test]
    fn normal_production_uses_exact_snapshot_but_diagnostic_trace_keeps_full_ring() {
        assert!(exact_production_snapshot_mode(true, false));
        assert!(!exact_production_snapshot_mode(true, true));
        assert!(!exact_production_snapshot_mode(false, false));
    }

    #[test]
    fn exact_snapshot_gate_copies_only_the_observed_projection_pass() {
        let unrelated = MainPassIdentity {
            pass_ordinal: 3,
            exact_draw_ordinal: None,
            projection_observed: false,
        };
        let missing_projection = MainPassIdentity {
            pass_ordinal: 0,
            exact_draw_ordinal: Some(6),
            projection_observed: false,
        };
        let exact = MainPassIdentity {
            pass_ordinal: 0,
            exact_draw_ordinal: Some(6),
            projection_observed: true,
        };

        assert!(!exact_snapshot_copy_decision(None, 0x10, None).unwrap());
        assert!(!exact_snapshot_copy_decision(Some(unrelated), 0x10, None).unwrap());
        assert!(!exact_snapshot_copy_decision(Some(missing_projection), 0x10, None).unwrap());
        assert!(exact_snapshot_copy_decision(Some(exact), 0x10, None).unwrap());
        let captured = ExactSnapshotRecord {
            texture_index: 0,
            unbind_ordinal_from_start: 1,
            source_resource_tag: 0x10,
            identity: exact,
        };
        assert!(!exact_snapshot_copy_decision(Some(exact), 0x10, Some(captured)).unwrap());
        assert!(exact_snapshot_copy_decision(Some(exact), 0x20, Some(captured)).is_err());
        assert!(
            exact_snapshot_copy_decision(
                Some(MainPassIdentity {
                    pass_ordinal: 1,
                    ..exact
                }),
                0x10,
                Some(captured),
            )
            .is_err()
        );
    }

    #[test]
    fn exact_snapshot_derives_pass_evidence_from_actual_unbind_total_without_guessing() {
        let identity = MainPassIdentity {
            pass_ordinal: 0,
            exact_draw_ordinal: Some(6),
            projection_observed: true,
        };
        let record = ExactSnapshotRecord {
            texture_index: 0,
            unbind_ordinal_from_start: 1,
            source_resource_tag: 0x10,
            identity,
        };
        let expected = ExactProductionDrawIdentity {
            pass_ordinal: 0,
            draw_ordinal: 6,
            projection_observed: true,
        };

        let six = select_exact_snapshot(Some(record), false, 6, Some(expected)).unwrap();
        let ten = select_exact_snapshot(Some(record), false, 10, Some(expected)).unwrap();
        assert_eq!(six.texture_index, 0);
        assert_eq!(six.unbind_ordinal_from_end, 6);
        assert_eq!(ten.unbind_ordinal_from_end, 10);
        assert!(select_exact_snapshot(None, false, 6, Some(expected)).is_err());
        assert!(select_exact_snapshot(Some(record), true, 6, Some(expected)).is_err());
        assert!(select_exact_snapshot(Some(record), false, 6, None).is_err());
        assert!(
            select_exact_snapshot(
                Some(ExactSnapshotRecord {
                    identity: MainPassIdentity {
                        pass_ordinal: 1,
                        ..identity
                    },
                    ..record
                }),
                false,
                6,
                Some(expected),
            )
            .is_err()
        );
    }

    #[test]
    fn duplicate_exact_claim_does_not_change_unbind_evidence() {
        let identity = MainPassIdentity {
            pass_ordinal: 0,
            exact_draw_ordinal: Some(6),
            projection_observed: true,
        };
        let source_resource_tag = 0x10;
        let mut unbind_total = 0u32;
        assert!(exact_snapshot_copy_decision(Some(identity), source_resource_tag, None).unwrap());
        unbind_total += 1;
        let record = ExactSnapshotRecord {
            texture_index: 0,
            unbind_ordinal_from_start: unbind_total,
            source_resource_tag,
            identity,
        };

        let duplicate_copies =
            exact_snapshot_copy_decision(Some(identity), source_resource_tag, Some(record))
                .unwrap();
        if duplicate_copies {
            unbind_total += 1;
        }
        assert_eq!(
            unbind_total, 1,
            "an idempotent claim must not change the ledger"
        );
        let expected = ExactProductionDrawIdentity {
            pass_ordinal: 0,
            draw_ordinal: 6,
            projection_observed: true,
        };
        let selected =
            select_exact_snapshot(Some(record), false, unbind_total, Some(expected)).unwrap();
        assert_eq!(selected.unbind_ordinal_from_end, 1);
    }

    #[test]
    fn optional_capture_budget_stops_after_the_requested_frame_count() {
        assert!(capture_budget_available(899, Some(900)));
        assert!(!capture_budget_available(900, Some(900)));
        assert!(capture_budget_available(u64::MAX, None));
    }

    #[test]
    fn lifecycle_warmup_tracks_qualified_presents_across_faster_raw_presents() {
        let start = std::time::Instant::now();
        let mut warmup = CaptureWarmup::new(7);
        for ordinal in 0..=8 {
            let ready = warmup.observe(
                7,
                start + std::time::Duration::from_millis(70 * ordinal),
                warmup_observation(true),
            );
            assert_eq!(ready, ordinal == 8);
            if ordinal < 8 {
                // Raw Presents without a deliverable depth/projection pair are expected between
                // the 15 Hz qualified samples and must not erase the qualified stream.
                assert!(!warmup.observe(
                    7,
                    start + std::time::Duration::from_millis(70 * ordinal + 10),
                    warmup_observation(false),
                ));
            }
        }
        assert_eq!(warmup.qualified_presents, 9);
        assert_eq!(warmup.continuity_resets, 0);
    }

    #[test]
    fn lifecycle_warmup_resets_after_a_gap_and_rejects_stale_generations() {
        let start = std::time::Instant::now();
        let mut warmup = CaptureWarmup::new(11);
        assert!(!warmup.observe(10, start, warmup_observation(true)));
        assert!(!warmup.observe(11, start, warmup_observation(true)));
        assert!(!warmup.observe(
            11,
            start + CAPTURE_WARMUP_MAX_PRESENT_GAP + std::time::Duration::from_millis(1),
            warmup_observation(true),
        ));
        for ordinal in 1..=7 {
            let ready = warmup.observe(
                11,
                start
                    + CAPTURE_WARMUP_MAX_PRESENT_GAP
                    + std::time::Duration::from_millis(1 + 100 * ordinal),
                warmup_observation(true),
            );
            assert_eq!(ready, ordinal == 7);
        }
        assert_eq!(warmup.continuity_resets, 1);
        assert!(!warmup.observe(
            11,
            start + std::time::Duration::from_secs(2),
            warmup_observation(false),
        ));
        assert_eq!(warmup.continuity_resets, 2);
    }

    #[test]
    fn lifecycle_warmup_timeout_diagnostics_name_the_actual_blockers() {
        let start = std::time::Instant::now();
        let mut warmup = CaptureWarmup::new(3);
        let mut blocked = warmup_observation(true);
        blocked.dsv_deliverable = false;
        blocked.camera.projection_ready = false;
        blocked.camera.player_sample_error = Some("Fallout actor root is null".into());
        assert!(!warmup.observe(3, start, blocked));
        let summary = warmup.diagnostic_summary();
        assert!(summary.contains("dsv_audit_not_deliverable"));
        assert!(summary.contains("exact_draw_projection_missing"));
        assert!(summary.contains("player_sampler=Fallout actor root is null"));
    }

    #[test]
    fn lifecycle_warmup_timeout_is_explicit_and_monotonic() {
        let start = std::time::Instant::now();
        let timeout =
            depth_lab::probe_control::capture_warmup_timeout(depth_lab::CAMERA_BINDING_SCHEMA_V2);
        assert!(!capture_warmup_timed_out(
            start,
            start + timeout - std::time::Duration::from_millis(1),
            timeout,
        ));
        assert!(capture_warmup_timed_out(start, start + timeout, timeout,));
        assert!(!capture_warmup_timed_out(
            start,
            start - std::time::Duration::from_millis(1),
            timeout,
        ));
    }

    #[test]
    fn cp2077_startup_waits_for_camera_history_without_relaxing_readiness() {
        let schema = depth_lab::CAMERA_BINDING_SCHEMA_V3;
        let timeout = depth_lab::probe_control::capture_warmup_timeout(schema);
        let start = std::time::Instant::now();
        for interval_ms in [100, 112, 125] {
            let mut warmup = CaptureWarmup::new(3);
            let required = depth_lab::d3d12_camera_local::MIN_PRESENTS as u64;
            let mut became_ready = false;
            for present in 1..=(required + 7) {
                let now = start + std::time::Duration::from_millis(present * interval_ms);
                let mut observation = warmup_observation(true);
                observation.camera.projection_ready = present >= required;
                let ready = warmup.observe(3, now, observation);
                assert_eq!(ready, present == required + 7);
                assert!(!capture_warmup_timed_out(start, now, timeout));
                became_ready |= ready;
            }
            assert!(became_ready);
            assert_eq!(warmup.qualified_presents, 8);
            assert_eq!(warmup.continuity_resets, 0);
        }
        assert!(capture_warmup_timed_out(start, start + timeout, timeout));
        let client_wait = std::time::Duration::from_millis(
            depth_lab::probe_control::client_ready_timeout_ms(15_000, schema),
        );
        assert_eq!(client_wait, timeout + std::time::Duration::from_secs(5));
    }

    #[test]
    fn new_d3d11_hooks_forward_every_original_argument_with_the_exact_abi() {
        let _runtime_serial = crate::dsv_discovery::lock_test_runtime();
        let _guard = ABI_FORWARD_LOCK.lock().unwrap();
        let context = 0x1111usize as *mut core::ffi::c_void;
        let buffer = 0x2222usize as *mut core::ffi::c_void;

        let old = ORIGINAL_DRAW_INDEXED_INSTANCED.swap(
            fake_draw_indexed_instanced as *const () as usize,
            Ordering::SeqCst,
        );
        unsafe { hooked_draw_indexed_instanced(context, 1, 2, 3, -4, 5) };
        ORIGINAL_DRAW_INDEXED_INSTANCED.store(old, Ordering::SeqCst);

        let old = ORIGINAL_DRAW_INSTANCED
            .swap(fake_draw_instanced as *const () as usize, Ordering::SeqCst);
        unsafe { hooked_draw_instanced(context, 6, 7, 8, 9) };
        ORIGINAL_DRAW_INSTANCED.store(old, Ordering::SeqCst);

        let old = ORIGINAL_DRAW_AUTO.swap(fake_draw_auto as *const () as usize, Ordering::SeqCst);
        unsafe { hooked_draw_auto(context) };
        ORIGINAL_DRAW_AUTO.store(old, Ordering::SeqCst);

        let old = ORIGINAL_DRAW_INDEXED_INSTANCED_INDIRECT.swap(
            fake_draw_indexed_indirect as *const () as usize,
            Ordering::SeqCst,
        );
        unsafe { hooked_draw_indexed_instanced_indirect(context, buffer, 10) };
        ORIGINAL_DRAW_INDEXED_INSTANCED_INDIRECT.store(old, Ordering::SeqCst);

        let old = ORIGINAL_DRAW_INSTANCED_INDIRECT
            .swap(fake_draw_indirect as *const () as usize, Ordering::SeqCst);
        unsafe { hooked_draw_instanced_indirect(context, buffer, 11) };
        ORIGINAL_DRAW_INSTANCED_INDIRECT.store(old, Ordering::SeqCst);

        let command_list = 0x7777usize as *mut core::ffi::c_void;
        let old = ORIGINAL_EXECUTE_COMMAND_LIST.swap(
            fake_execute_command_list as *const () as usize,
            Ordering::SeqCst,
        );
        unsafe { hooked_execute_command_list(context, command_list, BOOL(1)) };
        ORIGINAL_EXECUTE_COMMAND_LIST.store(old, Ordering::SeqCst);

        let render_target_views = 0x3333usize as *const *mut core::ffi::c_void;
        let depth_stencil_view = 0x4444usize as *mut core::ffi::c_void;
        let unordered_access_views = 0x5555usize as *const *mut core::ffi::c_void;
        let initial_counts = 0x6666usize as *const u32;
        let old = ORIGINAL_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS
            .swap(fake_om_uav as *const () as usize, Ordering::SeqCst);
        unsafe {
            hooked_om_set_render_targets_and_unordered_access_views(
                context,
                2,
                render_target_views,
                depth_stencil_view,
                3,
                4,
                unordered_access_views,
                initial_counts,
            )
        };
        ORIGINAL_OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS.store(old, Ordering::SeqCst);

        assert_eq!(
            *INDEXED_INSTANCED_CALL.lock().unwrap(),
            Some((0x1111, 1, 2, 3, -4, 5))
        );
        assert_eq!(*INSTANCED_CALL.lock().unwrap(), Some((0x1111, 6, 7, 8, 9)));
        assert_eq!(*AUTO_CALL.lock().unwrap(), Some(0x1111));
        assert_eq!(
            *INDEXED_INDIRECT_CALL.lock().unwrap(),
            Some((0x1111, 0x2222, 10))
        );
        assert_eq!(*INDIRECT_CALL.lock().unwrap(), Some((0x1111, 0x2222, 11)));
        assert_eq!(
            *EXECUTE_COMMAND_LIST_CALL.lock().unwrap(),
            Some((0x1111, 0x7777, true))
        );
        assert_eq!(
            *OM_UAV_CALL.lock().unwrap(),
            Some((0x1111, 2, 0x3333, 0x4444, 3, 4, 0x5555, 0x6666))
        );
    }

    #[test]
    fn command_list_route_only_replays_on_the_bound_immediate_context() {
        assert_eq!(
            dsv_command_list_route(false, 0x1111, 0x1111),
            DsvCommandListRoute::Ignore
        );
        assert_eq!(
            dsv_command_list_route(true, 0x1111, 0x2222),
            DsvCommandListRoute::ForeignContext
        );
        assert_eq!(
            dsv_command_list_route(true, 0x1111, 0x1111),
            DsvCommandListRoute::ReplayOnGameContext
        );
    }

    #[test]
    fn deferred_ledger_rejects_the_first_partial_generation_then_accepts_a_clean_boundary() {
        let (_device, context) = warp_device_with_deferred_context();
        let mut ledger = DeferredDsvLedger::default();
        ledger
            .push_event(context.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();

        let mut first = None;
        unsafe { context.FinishCommandList(false, Some(&mut first)) }.unwrap();
        let first = first.unwrap();
        assert!(
            !ledger
                .seal_command_list(context.as_raw(), first.clone(), false)
                .unwrap(),
            "the first generation can contain commands recorded before hook installation"
        );
        assert!(
            !ledger
                .command_lists
                .get(&(first.as_raw() as usize))
                .unwrap()
                .complete
        );

        ledger
            .push_event(context.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();
        let mut second = None;
        unsafe { context.FinishCommandList(false, Some(&mut second)) }.unwrap();
        let second = second.unwrap();
        assert!(
            ledger
                .seal_command_list(context.as_raw(), second.clone(), false)
                .unwrap()
        );
        let sealed = ledger
            .command_lists
            .get(&(second.as_raw() as usize))
            .unwrap();
        assert!(sealed.complete);
        assert_eq!(sealed.events.len(), 1);
        assert!(matches!(sealed.events[0], DeferredDsvEvent::Draw));
    }

    #[test]
    fn deferred_seal_rejects_partial_actual_context_vtable_drift_after_initial_admission() {
        let (_device, context) = warp_device_with_deferred_context();
        let mut ledger = DeferredDsvLedger::default();
        ledger.ensure_context(context.as_raw()).unwrap();
        ledger
            .revalidate_context_hook_coverage(context.as_raw())
            .unwrap();
        {
            let recording = ledger
                .contexts
                .get_mut(&(context.as_raw() as usize))
                .unwrap();
            recording.state_known = true;
            recording.generation_complete = true;
        }
        ledger
            .push_event(context.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();

        let mut command_list = None;
        unsafe { context.FinishCommandList(false, Some(&mut command_list)) }.unwrap();
        let command_list = command_list.unwrap();
        let command_list_key = command_list.as_raw() as usize;

        let (entry_count, _) = context_shadow_vtable_shape(&context);
        assert!(entry_count > d3d11_context::OM_SET_RENDER_TARGETS);
        let object_vtable = context.as_raw().cast::<*mut usize>();
        let original_vtable = unsafe { *object_vtable };
        let mut partial_vtable = unsafe {
            core::slice::from_raw_parts(original_vtable, entry_count)
                .to_vec()
                .into_boxed_slice()
        };
        partial_vtable[d3d11_context::OM_SET_RENDER_TARGETS] =
            partial_vtable[d3d11_context::OM_SET_RENDER_TARGETS].wrapping_add(1);

        // Model a component switching only this actual context to a copied/partial table after its
        // initial admission. Restore the real owner pointer before inspecting the result or dropping
        // any COM owner, even if sealing correctly rejects the drift.
        unsafe { *object_vtable = partial_vtable.as_mut_ptr() };
        let result = ledger.seal_command_list(context.as_raw(), command_list, false);
        unsafe { *object_vtable = original_vtable };

        let error = result.unwrap_err();
        assert!(error.starts_with(DEFERRED_HOOK_COVERAGE_FAILURE));
        assert!(error.contains("vtable owner changed after admission"));
        let sealed = ledger.command_lists.get(&command_list_key).unwrap();
        assert!(!sealed.complete);
        assert!(sealed.events.is_empty());
        assert!(
            ledger
                .complete_command_list_events(command_list_key as *mut core::ffi::c_void)
                .is_none(),
            "a partial actual-context table must never produce a replayable list"
        );
    }

    #[test]
    fn deferred_finish_true_preserves_unknown_state_until_a_reset_boundary() {
        let (_device, context) = warp_device_with_deferred_context();
        let mut ledger = DeferredDsvLedger::default();

        let mut first = None;
        unsafe { context.FinishCommandList(true, Some(&mut first)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(context.as_raw(), first.unwrap(), true)
                .unwrap()
        );
        let recording = ledger.contexts.get(&(context.as_raw() as usize)).unwrap();
        assert!(!recording.state_known);
        assert!(!recording.generation_complete);

        let mut reset = None;
        unsafe { context.FinishCommandList(false, Some(&mut reset)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(context.as_raw(), reset.unwrap(), false)
                .unwrap()
        );
        let recording = ledger.contexts.get(&(context.as_raw() as usize)).unwrap();
        assert!(recording.state_known);
        assert!(recording.generation_complete);
    }

    #[test]
    fn deferred_warmup_absorbs_nineteen_unknown_first_generations() {
        let (device, first) = warp_device_with_deferred_context();
        let mut contexts = vec![first];
        for _ in 1..19 {
            let mut context = None;
            unsafe { device.CreateDeferredContext(0, Some(&mut context)) }.unwrap();
            contexts.push(context.unwrap());
        }
        let mut ledger = DeferredDsvLedger::default();

        for context in &contexts {
            let mut list = None;
            unsafe { context.FinishCommandList(false, Some(&mut list)) }.unwrap();
            assert!(
                !ledger
                    .seal_command_list(context.as_raw(), list.unwrap(), false)
                    .unwrap(),
                "the excluded warmup generation stays opaque"
            );
        }
        for context in &contexts {
            ledger
                .push_event(context.as_raw(), DeferredDsvEvent::Draw)
                .unwrap();
            let mut list = None;
            unsafe { context.FinishCommandList(false, Some(&mut list)) }.unwrap();
            assert!(
                ledger
                    .seal_command_list(context.as_raw(), list.unwrap(), false)
                    .unwrap(),
                "the first formally eligible generation has a proven FALSE boundary"
            );
        }
        assert_eq!(
            ledger
                .command_lists
                .values()
                .filter(|recording| recording.complete)
                .count(),
            19
        );
    }

    #[test]
    fn deferred_nested_lists_merge_known_events_and_taint_unknown_sources() {
        let (device, source_context) = warp_device_with_deferred_context();
        let mut destination = None;
        unsafe { device.CreateDeferredContext(0, Some(&mut destination)) }.unwrap();
        let destination = destination.unwrap();
        let mut ledger = DeferredDsvLedger::default();

        let mut boundary = None;
        unsafe { source_context.FinishCommandList(false, Some(&mut boundary)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(source_context.as_raw(), boundary.unwrap(), false)
                .unwrap()
        );
        ledger
            .push_event(source_context.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();
        let mut source = None;
        unsafe { source_context.FinishCommandList(false, Some(&mut source)) }.unwrap();
        let source = source.unwrap();
        assert!(
            ledger
                .seal_command_list(source_context.as_raw(), source.clone(), false)
                .unwrap()
        );

        let mut boundary = None;
        unsafe { destination.FinishCommandList(false, Some(&mut boundary)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(destination.as_raw(), boundary.unwrap(), false)
                .unwrap()
        );
        assert!(
            ledger
                .merge_command_list(destination.as_raw(), source.as_raw(), false)
                .unwrap()
        );
        let mut merged = None;
        unsafe { destination.FinishCommandList(false, Some(&mut merged)) }.unwrap();
        let merged = merged.unwrap();
        assert!(
            ledger
                .seal_command_list(destination.as_raw(), merged.clone(), false)
                .unwrap()
        );
        let merged = ledger
            .command_lists
            .get(&(merged.as_raw() as usize))
            .unwrap();
        assert_eq!(merged.events.len(), 3);
        assert!(matches!(merged.events[0], DeferredDsvEvent::Bind(None)));
        assert!(matches!(merged.events[1], DeferredDsvEvent::Draw));
        assert!(matches!(merged.events[2], DeferredDsvEvent::Bind(None)));

        assert!(
            !ledger
                .merge_command_list(
                    destination.as_raw(),
                    0xDEADusize as *mut core::ffi::c_void,
                    false,
                )
                .unwrap()
        );
        let mut opaque = None;
        unsafe { destination.FinishCommandList(false, Some(&mut opaque)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(destination.as_raw(), opaque.unwrap(), false)
                .unwrap()
        );
    }

    #[test]
    fn deferred_nested_restore_true_never_collapses_unknown_state_to_none() {
        let (device, source_context) = warp_device_with_deferred_context();
        let mut destination = None;
        unsafe { device.CreateDeferredContext(0, Some(&mut destination)) }.unwrap();
        let destination = destination.unwrap();
        let mut ledger = DeferredDsvLedger::default();

        let mut boundary = None;
        unsafe { source_context.FinishCommandList(false, Some(&mut boundary)) }.unwrap();
        ledger
            .seal_command_list(source_context.as_raw(), boundary.unwrap(), false)
            .unwrap();
        ledger
            .push_event(source_context.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();
        let mut source = None;
        unsafe { source_context.FinishCommandList(false, Some(&mut source)) }.unwrap();
        let source = source.unwrap();
        assert!(
            ledger
                .seal_command_list(source_context.as_raw(), source.clone(), false)
                .unwrap()
        );

        assert!(
            ledger
                .merge_command_list(destination.as_raw(), source.as_raw(), true)
                .unwrap()
        );
        let destination_state = ledger
            .contexts
            .get(&(destination.as_raw() as usize))
            .unwrap();
        assert!(!destination_state.state_known);
        assert!(!destination_state.generation_complete);
        let mut first = None;
        unsafe { destination.FinishCommandList(true, Some(&mut first)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(destination.as_raw(), first.unwrap(), true)
                .unwrap()
        );
        ledger
            .push_event(destination.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();
        let mut second = None;
        unsafe { destination.FinishCommandList(false, Some(&mut second)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(destination.as_raw(), second.unwrap(), false)
                .unwrap(),
            "Finish(TRUE) must preserve the unknown entry state into the next generation"
        );
    }

    #[test]
    fn deferred_query_or_unmodeled_state_command_keeps_the_list_opaque() {
        let (_device, context) = warp_device_with_deferred_context();
        let mut ledger = DeferredDsvLedger::default();
        let mut boundary = None;
        unsafe { context.FinishCommandList(false, Some(&mut boundary)) }.unwrap();
        ledger
            .seal_command_list(context.as_raw(), boundary.unwrap(), false)
            .unwrap();

        ledger.taint_context(context.as_raw(), false).unwrap();
        ledger
            .push_event(context.as_raw(), DeferredDsvEvent::Draw)
            .unwrap();
        let mut list = None;
        unsafe { context.FinishCommandList(false, Some(&mut list)) }.unwrap();
        assert!(
            !ledger
                .seal_command_list(context.as_raw(), list.unwrap(), false)
                .unwrap()
        );
    }

    #[test]
    fn deferred_ledger_reset_releases_all_bounded_owners() {
        let (_device, context) = warp_device_with_deferred_context();
        let mut ledger = DeferredDsvLedger::default();
        let mut list = None;
        unsafe { context.FinishCommandList(false, Some(&mut list)) }.unwrap();
        ledger
            .seal_command_list(context.as_raw(), list.unwrap(), false)
            .unwrap();
        assert!(!ledger.contexts.is_empty());
        assert!(!ledger.command_lists.is_empty());
        ledger.reset();
        assert!(ledger.contexts.is_empty());
        assert!(ledger.command_lists.is_empty());
        assert_eq!(ledger.retained_events, 0);
    }

    #[test]
    fn deferred_event_bound_taints_without_growing_the_recording() {
        let (_device, context) = warp_device_with_deferred_context();
        let mut ledger = DeferredDsvLedger::default();
        ledger.ensure_context(context.as_raw()).unwrap();
        let recording = ledger
            .contexts
            .get_mut(&(context.as_raw() as usize))
            .unwrap();
        recording.state_known = true;
        recording.generation_complete = true;
        recording.events = vec![DeferredDsvEvent::Draw; MAX_DSV_DEFERRED_EVENTS_PER_RECORDING];
        ledger.retained_events = MAX_DSV_DEFERRED_EVENTS_PER_RECORDING;

        assert!(
            ledger
                .push_event(context.as_raw(), DeferredDsvEvent::Draw)
                .is_err()
        );
        let recording = ledger.contexts.get(&(context.as_raw() as usize)).unwrap();
        assert_eq!(
            recording.events.len(),
            MAX_DSV_DEFERRED_EVENTS_PER_RECORDING
        );
        assert!(!recording.generation_complete);
    }

    #[test]
    fn deferred_replay_preserves_event_order_and_supports_repeated_execution() {
        let (device, _context) = warp_device_with_deferred_context();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 64,
            Height: 64,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_D24_UNORM_S8_UINT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
        let texture = texture.unwrap();
        let mut view = None;
        unsafe { device.CreateDepthStencilView(&texture, None, Some(&mut view)) }.unwrap();
        let view = view.unwrap();
        let snapshot = inspect_deferred_dsv(view.as_raw()).unwrap();
        let events = vec![
            DeferredDsvEvent::Bind(Some(snapshot.clone())),
            DeferredDsvEvent::Clear(snapshot),
            DeferredDsvEvent::Draw,
            DeferredDsvEvent::Bind(None),
        ];

        let mut tracker = DsvFrameTracker::default();
        tracker.reset_for_discovery();
        tracker.replay_deferred(&events, false);
        tracker.replay_deferred(&events, false);
        let frame = tracker.finish_discovery_frame(7);
        assert_eq!(frame.candidates.len(), 1);
        let candidate = &frame.candidates[0];
        assert_eq!(candidate.bind_count, 2);
        assert_eq!(candidate.unbind_count, 2);
        assert_eq!(candidate.pass_count, 2);
        assert_eq!(candidate.draw_calls, 2);
        assert_eq!(candidate.clear_count, 2);
        assert_eq!(candidate.completed_passes.len(), 2);
        assert_eq!(candidate.completed_passes[0].0, 1);
        assert_eq!(candidate.completed_passes[0].1, 1);
        assert_eq!(candidate.completed_passes[1].0, 1);
        assert_eq!(candidate.completed_passes[1].1, 1);
    }

    #[test]
    fn dsv_observation_window_starts_once_after_a_clean_hook_bootstrap_boundary() {
        let _runtime_serial = crate::dsv_discovery::lock_test_runtime();
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dm-dsv-warmup-boundary-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable_sha256 = format!("sha256:{}", "a".repeat(64));
        crate::dsv_discovery::start(
            depth_lab::DsvDiscoveryConfig {
                schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
                discovery_id: "warmup-boundary".into(),
                executable_sha256: executable_sha256.clone(),
                evidence_root: root.display().to_string(),
                limits: depth_lab::DsvDiscoveryLimits::default(),
            },
            &root.join("run"),
            &executable_sha256,
        )
        .unwrap();
        let (device, _deferred) = warp_device_with_deferred_context();
        let immediate = unsafe { device.GetImmediateContext() }.unwrap();
        let old_context =
            super::GAME_IMMEDIATE_CONTEXT.swap(immediate.as_raw() as usize, Ordering::SeqCst);
        let old = super::DSV_DISCOVERY_OBSERVATION_STARTED.swap(false, Ordering::SeqCst);
        let old_warmup = super::DSV_DISCOVERY_DEFERRED_WARMUP_STARTED.swap(false, Ordering::SeqCst);
        let old_complete = super::DSV_DISCOVERY_OBSERVATION_COMPLETE.swap(false, Ordering::SeqCst);
        assert!(!super::begin_dsv_discovery_observation_after_hook_bootstrap().unwrap());
        assert!(super::begin_dsv_discovery_observation_after_hook_bootstrap().unwrap());
        assert!(!super::begin_dsv_discovery_observation_after_hook_bootstrap().unwrap());
        crate::dsv_discovery::stop().unwrap();
        super::dsv_frame_tracker().lock().unwrap().reset();
        super::reset_deferred_dsv_ledger().unwrap();
        super::GAME_IMMEDIATE_CONTEXT.store(old_context, Ordering::SeqCst);
        super::DSV_DISCOVERY_OBSERVATION_STARTED.store(old, Ordering::SeqCst);
        super::DSV_DISCOVERY_DEFERRED_WARMUP_STARTED.store(old_warmup, Ordering::SeqCst);
        super::DSV_DISCOVERY_OBSERVATION_COMPLETE.store(old_complete, Ordering::SeqCst);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn dsv_discovery_admission_closes_exactly_at_the_configured_frame_limit() {
        assert!(!dsv_discovery_frame_limit_reached(0, 600));
        assert!(!dsv_discovery_frame_limit_reached(599, 600));
        assert!(dsv_discovery_frame_limit_reached(600, 600));
        assert!(dsv_discovery_frame_limit_reached(601, 600));
        assert!(!dsv_discovery_frame_limit_reached(u32::MAX, 0));
    }

    #[test]
    fn active_dsv_discovery_seals_command_list_calls_as_durable_blockers() {
        let _runtime_serial = crate::dsv_discovery::lock_test_runtime();
        let _abi_serial = ABI_FORWARD_LOCK.lock().unwrap();
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "dm-dsv-command-list-hook-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable_sha256 = format!("sha256:{}", "a".repeat(64));
        let config = |discovery_id: &str| depth_lab::DsvDiscoveryConfig {
            schema: depth_lab::DSV_DISCOVERY_CONFIG_SCHEMA_V2.into(),
            discovery_id: discovery_id.into(),
            executable_sha256: executable_sha256.clone(),
            evidence_root: root.display().to_string(),
            limits: depth_lab::DsvDiscoveryLimits::default(),
        };
        let expected_context = 0x1111usize;
        let old_context = super::GAME_IMMEDIATE_CONTEXT.swap(expected_context, Ordering::SeqCst);
        let old_observation =
            super::DSV_DISCOVERY_OBSERVATION_STARTED.swap(false, Ordering::SeqCst);
        let old_original = ORIGINAL_EXECUTE_COMMAND_LIST.swap(
            fake_execute_command_list as *const () as usize,
            Ordering::SeqCst,
        );

        crate::dsv_discovery::start(
            config("same-context"),
            &root.join("same-context"),
            &executable_sha256,
        )
        .unwrap();
        unsafe {
            hooked_execute_command_list(
                expected_context as *mut core::ffi::c_void,
                0x1000usize as *mut core::ffi::c_void,
                BOOL(1),
            )
        };
        assert_eq!(
            *EXECUTE_COMMAND_LIST_CALL.lock().unwrap(),
            Some((expected_context, 0x1000, true)),
            "pre-boundary calls must still forward even though they are outside evidence"
        );
        super::DSV_DISCOVERY_OBSERVATION_STARTED.store(true, Ordering::SeqCst);
        unsafe {
            hooked_execute_command_list(
                expected_context as *mut core::ffi::c_void,
                0x2000usize as *mut core::ffi::c_void,
                BOOL(0),
            )
        };
        assert_eq!(
            *EXECUTE_COMMAND_LIST_CALL.lock().unwrap(),
            Some((expected_context, 0x2000, false))
        );
        unsafe {
            hooked_execute_command_list(
                expected_context as *mut core::ffi::c_void,
                0x3000usize as *mut core::ffi::c_void,
                BOOL(1),
            )
        };
        assert_eq!(
            *EXECUTE_COMMAND_LIST_CALL.lock().unwrap(),
            Some((expected_context, 0x3000, true))
        );
        let same_path = crate::dsv_discovery::stop().unwrap();
        let same = depth_lab::read_report_bounded(&same_path, &root).unwrap();
        assert_eq!(same.summary.runtime.hook_integrity_failures, 2);
        assert_eq!(
            same.summary
                .runtime
                .hook_integrity_failure_reasons
                .as_ref()
                .unwrap()
                .unsupported_execute_command_list,
            2
        );
        assert_eq!(
            same.summary
                .runtime
                .hook_integrity_failure_reasons
                .as_ref()
                .unwrap()
                .vtable_or_slot_drift,
            0
        );
        assert!(
            same.blockers
                .contains(&depth_lab::DsvDiscoveryBlocker::HookIntegrityFailure)
        );
        assert!(!same.complete);

        crate::dsv_discovery::start(
            config("foreign-context"),
            &root.join("foreign-context"),
            &executable_sha256,
        )
        .unwrap();
        super::DSV_DISCOVERY_OBSERVATION_STARTED.store(true, Ordering::SeqCst);
        unsafe {
            hooked_execute_command_list(
                0x4444usize as *mut core::ffi::c_void,
                0x5000usize as *mut core::ffi::c_void,
                BOOL(0),
            )
        };
        let foreign_path = crate::dsv_discovery::stop().unwrap();
        let foreign = depth_lab::read_report_bounded(&foreign_path, &root).unwrap();
        assert_eq!(foreign.summary.runtime.hook_dropped_events, 1);
        assert!(
            foreign
                .blockers
                .contains(&depth_lab::DsvDiscoveryBlocker::HookObservationDropped)
        );
        assert!(!foreign.complete);

        ORIGINAL_EXECUTE_COMMAND_LIST.store(old_original, Ordering::SeqCst);
        super::GAME_IMMEDIATE_CONTEXT.store(old_context, Ordering::SeqCst);
        super::DSV_DISCOVERY_OBSERVATION_STARTED.store(old_observation, Ordering::SeqCst);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn hook_entry_counters_distinguish_every_draw_and_om_method() {
        let counters = CameraHookEntryCounters::new();
        for method in CameraDrawMethod::ALL {
            assert_eq!(counters.record_draw(method), 1);
            assert_eq!(counters.record_draw(method), 2);
            assert_eq!(counters.record_foreign_draw(method), 1);
        }
        for method in OmBindMethod::ALL {
            assert_eq!(counters.record_om_bind(method), 1);
            assert_eq!(counters.record_om_bind(method), 2);
            assert_eq!(counters.record_foreign_om_bind(method), 1);
        }
        let snapshot = counters.take();
        for method in CameraDrawMethod::ALL {
            assert_eq!(snapshot.draws[method as usize], 2, "{}", method.label());
            assert_eq!(
                snapshot.foreign_draws[method as usize],
                1,
                "{}",
                method.label()
            );
        }
        for method in OmBindMethod::ALL {
            assert_eq!(snapshot.om_binds[method as usize], 2, "{}", method.label());
            assert_eq!(
                snapshot.foreign_om_binds[method as usize],
                1,
                "{}",
                method.label()
            );
        }
        assert_eq!(counters.take().draws, [0; 7]);
        assert_eq!(counters.take().om_binds, [0; 2]);
        assert_eq!(counters.take().foreign_draws, [0; 7]);
        assert_eq!(counters.take().foreign_om_binds, [0; 2]);
    }

    #[test]
    fn outer_draw_gate_distinguishes_context_drift_and_integrity_failure() {
        assert_eq!(
            camera_draw_entry_route(0x1000, 0x1000, true, false),
            CameraDrawEntryRoute::Accept
        );
        assert_eq!(
            camera_draw_entry_route(0x1000, 0x2000, true, false),
            CameraDrawEntryRoute::ForeignContext
        );
        assert_eq!(
            camera_draw_entry_route(0x1000, 0x1000, true, true),
            CameraDrawEntryRoute::HookIntegrityFailed
        );
        // Camera 关闭时不消费上一 run 留下的完整性状态。
        assert_eq!(
            camera_draw_entry_route(0x1000, 0x1000, false, true),
            CameraDrawEntryRoute::Accept
        );
    }

    #[test]
    fn camera_modes_and_dsv_discovery_require_the_shadow_vtable() {
        assert!(!camera_capture_requires_shadow(false, false));
        assert!(camera_capture_requires_shadow(true, false));
        assert!(camera_capture_requires_shadow(false, true));
        assert!(camera_capture_requires_shadow(true, true));

        assert!(!depth_observation_requires_shadow(false, false, false));
        assert!(depth_observation_requires_shadow(true, false, false));
        assert!(depth_observation_requires_shadow(false, true, false));
        assert!(depth_observation_requires_shadow(false, false, true));
    }

    #[test]
    fn present_transition_only_matches_a_complete_draw_slot_restore() {
        let before = BackingHookSnapshot {
            draw_replacement: 7,
            om_replacement: 2,
            ..Default::default()
        };
        let after = BackingHookSnapshot {
            draw_original: 7,
            om_replacement: 2,
            ..Default::default()
        };
        assert!(backing_draws_restored_inside_present(before, after));

        let partial = BackingHookSnapshot {
            draw_original: 6,
            draw_unknown: 1,
            om_replacement: 2,
            ..Default::default()
        };
        assert!(!backing_draws_restored_inside_present(before, partial));
    }

    #[test]
    fn om_uav_keep_preserves_dsv_and_reacquire_routes_keep_phase_strictness_separate() {
        assert_eq!(
            om_uav_depth_binding_action(
                windows::Win32::Graphics::Direct3D11::D3D11_KEEP_RENDER_TARGETS_AND_DEPTH_STENCIL,
                std::ptr::null_mut(),
            ),
            OmDepthBindingAction::Keep
        );
        let dsv = 0x1234usize as *mut core::ffi::c_void;
        assert_eq!(
            om_uav_depth_binding_action(1, dsv),
            OmDepthBindingAction::Set(dsv)
        );
        assert_eq!(
            dsv_draw_reacquire_route(0, 0).unwrap(),
            DsvDrawReacquireRoute::RuntimeDescriptor
        );
        assert_eq!(
            dsv_draw_reacquire_route(0x1000, 0x1001).unwrap(),
            DsvDrawReacquireRoute::PhaseResetExactPair(dsv_pair(0x1000, 0x1001))
        );
        assert!(dsv_draw_reacquire_route(0x1000, 0).is_err());

        let expected = CameraDiscoveryRenderIdentity {
            width: 1920,
            height: 1080,
            sample_count: 1,
            formats: vec![DiscoveryDepthFormat::R24G8Typeless],
        };
        let mut selection = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::LegacyRender(expected)),
            ..Default::default()
        };
        let first = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        let second = observed_dsv_with_view(0x2000, 0x2001, 1920, 1080);
        assert!(accept_selected_depth_resource(&mut selection, &first).unwrap());
        assert!(accept_selected_depth_resource(&mut selection, &second).is_err());
        assert!(selection.ambiguous);

        let mut serial_replacement = DepthSelectionState {
            expected: selection.expected.clone(),
            ..Default::default()
        };
        assert!(accept_selected_depth_resource(&mut serial_replacement, &first).unwrap());
        serial_replacement.note_explicit_unbind(dsv_pair(0x1000, 0x1001));
        let runtime_route = dsv_draw_reacquire_route(0, 0).unwrap();
        assert!(
            accept_reacquired_depth_resource(
                &mut serial_replacement,
                &second,
                runtime_route.expected_pair(),
            )
            .unwrap()
        );
        assert_eq!(
            serial_replacement.selected_pair,
            Some(dsv_pair(0x2000, 0x2001))
        );
        assert!(!serial_replacement.ambiguous);
        serial_replacement.note_explicit_unbind(dsv_pair(0x2000, 0x2001));
        assert!(accept_selected_depth_resource(&mut serial_replacement, &first).is_err());
        assert!(
            serial_replacement.ambiguous,
            "retired pair ABA must latch ambiguity"
        );

        let mut strict_phase_reacquire = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::ExactDescriptor(first.descriptor)),
            ..Default::default()
        };
        assert!(accept_selected_depth_resource(&mut strict_phase_reacquire, &first).unwrap());
        strict_phase_reacquire.note_explicit_unbind(dsv_pair(0x1000, 0x1001));
        assert!(
            accept_reacquired_depth_resource(
                &mut strict_phase_reacquire,
                &second,
                Some(dsv_pair(0x1000, 0x1001)),
            )
            .is_err()
        );
        assert!(strict_phase_reacquire.ambiguous);
        assert!(
            accept_reacquired_depth_resource(
                &mut strict_phase_reacquire,
                &first,
                Some(dsv_pair(0x1000, 0x1001)),
            )
            .is_err(),
            "an unexpected phase-reset pair must permanently prevent recovery to the old pair"
        );
    }

    #[test]
    fn ets2_d32s8_resource_and_view_formats_are_explicitly_supported() {
        let expected = [
            DiscoveryDepthFormat::R32G8X24Typeless,
            DiscoveryDepthFormat::D32FloatS8X24Uint,
        ];
        assert!(depth_format_matches(
            &expected,
            DXGI_FORMAT_R32G8X24_TYPELESS
        ));
        assert!(depth_format_matches(
            &expected,
            DXGI_FORMAT_D32_FLOAT_S8X24_UINT
        ));
        assert!(!depth_format_matches(&expected, DXGI_FORMAT_R24G8_TYPELESS));
        assert_eq!(
            shader_readable_snapshot_format(DXGI_FORMAT_D32_FLOAT_S8X24_UINT).unwrap(),
            DXGI_FORMAT_R32G8X24_TYPELESS
        );
        assert_eq!(
            shader_readable_snapshot_format(DXGI_FORMAT_R32G8X24_TYPELESS).unwrap(),
            DXGI_FORMAT_R32G8X24_TYPELESS
        );
    }

    fn discovery_probe_config(phase_id: &str) -> ProbeConfig {
        ProbeConfig {
            enabled: true,
            output_dir: "C:/synthetic/run".into(),
            unbind_from_end: 4,
            max_frames: Some(1),
            present_only: false,
            watermark_only: false,
            writer_bypass: false,
            target_depth_hz: Some(15.0),
            lane_fps: None,
            camera_probe: true,
            camera_discovery: Some(CameraDiscoveryConfig {
                schema: CAMERA_DISCOVERY_SCHEMA_V1.into(),
                phase_id: phase_id.into(),
                executable_sha256: format!("sha256:{}", "a".repeat(64)),
                render: Some(CameraDiscoveryRenderIdentity {
                    width: 1920,
                    height: 1080,
                    sample_count: 1,
                    formats: vec![DiscoveryDepthFormat::R24G8Typeless],
                }),
                dsv_selection: None,
                sample_every_n_presents: 30,
                max_main_passes_per_present: 1,
                draw_window_start: 0,
                draw_window_count: 128,
                comparison_camera_reference: None,
            }),
            representative_scene_attestation_sha256: None,
            dsv_discovery: None,
            camera_production: None,
            depth_profile_path: None,
            depth_profile_sha256: None,
            session_id: "synthetic-session".into(),
        }
    }

    #[test]
    fn phase_reset_accepts_sampling_changes_but_rejects_identity_drift() {
        let active = discovery_probe_config("A1");
        let mut next = discovery_probe_config("A2");
        next.camera_discovery.as_mut().unwrap().draw_window_start = 128;
        assert_eq!(
            camera_discovery_phase_reset_config(&active, &next)
                .unwrap()
                .phase_id,
            "A2"
        );

        let mut render_drift = next.clone();
        render_drift
            .camera_discovery
            .as_mut()
            .unwrap()
            .render
            .as_mut()
            .unwrap()
            .width = 2560;
        assert!(camera_discovery_phase_reset_config(&active, &render_drift).is_err());

        let mut executable_drift = next.clone();
        executable_drift
            .camera_discovery
            .as_mut()
            .unwrap()
            .executable_sha256 = format!("sha256:{}", "b".repeat(64));
        assert!(camera_discovery_phase_reset_config(&active, &executable_drift).is_err());

        let mut output_drift = next;
        output_drift.output_dir = "C:/synthetic/other-run".into();
        assert!(camera_discovery_phase_reset_config(&active, &output_drift).is_err());

        let mut v3_active = discovery_probe_config("V3-A1");
        let v3 = v3_active.camera_discovery.as_mut().unwrap();
        v3.schema = CAMERA_DISCOVERY_SCHEMA_V3.into();
        v3.render = None;
        v3.dsv_selection = Some(CameraDiscoveryDsvSelection {
            evidence_root: "C:/evidence".into(),
            attestation_locator: "selection.json".into(),
            attestation_sha256: format!("sha256:{}", "c".repeat(64)),
        });
        let mut v3_next = v3_active.clone();
        v3_next.camera_discovery.as_mut().unwrap().phase_id = "V3-A2".into();
        assert!(camera_discovery_phase_reset_config(&v3_active, &v3_next).is_ok());
        v3_next
            .camera_discovery
            .as_mut()
            .unwrap()
            .dsv_selection
            .as_mut()
            .unwrap()
            .attestation_locator = "other.json".into();
        assert!(camera_discovery_phase_reset_config(&v3_active, &v3_next).is_err());

        let mut hash_drift = v3_active.clone();
        hash_drift
            .camera_discovery
            .as_mut()
            .unwrap()
            .dsv_selection
            .as_mut()
            .unwrap()
            .attestation_sha256 = format!("sha256:{}", "d".repeat(64));
        assert!(camera_discovery_phase_reset_config(&v3_active, &hash_drift).is_err());

        let mut root_drift = v3_active.clone();
        root_drift
            .camera_discovery
            .as_mut()
            .unwrap()
            .dsv_selection
            .as_mut()
            .unwrap()
            .evidence_root = "C:/other-evidence".into();
        assert!(camera_discovery_phase_reset_config(&v3_active, &root_drift).is_err());
    }

    #[test]
    fn phase_reset_preserves_or_latches_only_a_unique_matching_dsv() {
        let render = discovery_probe_config("A1")
            .camera_discovery
            .unwrap()
            .render
            .unwrap();
        let expected = DepthSelectionExpectation::LegacyRender(render.clone());
        let pair = dsv_pair(0x1000, 0x1001);
        let selected = DepthSelectionState {
            expected: Some(expected.clone()),
            selected_pair: Some(pair),
            ..Default::default()
        };
        assert_eq!(
            phase_reset_depth_recovery(&selected, Some(pair), &expected).unwrap(),
            PhaseDepthRecovery::PreserveCurrent { pair }
        );
        assert_eq!(
            phase_reset_depth_recovery(&selected, None, &expected).unwrap(),
            PhaseDepthRecovery::ReacquireOnDraw { pair }
        );
        assert!(
            phase_reset_depth_recovery(&selected, Some(dsv_pair(0x2000, 0x2001)), &expected)
                .is_err()
        );

        let not_selected = DepthSelectionState {
            expected: Some(expected.clone()),
            ..Default::default()
        };
        assert_eq!(
            phase_reset_depth_recovery(&not_selected, None, &expected).unwrap(),
            PhaseDepthRecovery::AwaitInitialSelection
        );

        let mut changed_render = render.clone();
        changed_render.height = 1200;
        let changed = DepthSelectionExpectation::LegacyRender(changed_render);
        assert!(phase_reset_depth_recovery(&selected, Some(pair), &changed).is_err());

        let ambiguous = DepthSelectionState {
            ambiguous: true,
            ..selected
        };
        assert!(phase_reset_depth_recovery(&ambiguous, Some(pair), &expected).is_err());

        let descriptor = observed_dsv(0x3000, 1920, 1080).descriptor;
        let exact = DepthSelectionExpectation::ExactDescriptor(descriptor);
        let exact_selected = DepthSelectionState {
            expected: Some(exact.clone()),
            selected_pair: Some(pair),
            ..Default::default()
        };
        assert!(phase_reset_depth_recovery(&exact_selected, Some(pair), &exact).is_ok());
        let mut drifted_descriptor = descriptor;
        drifted_descriptor.misc_flags = 1;
        assert!(
            phase_reset_depth_recovery(
                &exact_selected,
                Some(pair),
                &DepthSelectionExpectation::ExactDescriptor(drifted_descriptor),
            )
            .is_err()
        );
    }

    #[test]
    fn probe_draw_gate_reacquires_pending_phase_before_dispatch() {
        let render = discovery_probe_config("PREP")
            .camera_discovery
            .unwrap()
            .render
            .unwrap();
        let expected = DepthSelectionExpectation::LegacyRender(render);
        let selection = DepthSelectionState {
            expected: Some(expected.clone()),
            selected_pair: Some(dsv_pair(0x1000, 0x1001)),
            ..Default::default()
        };
        let recovery = phase_reset_depth_recovery(&selection, None, &expected).unwrap();
        let pending = matches!(recovery, PhaseDepthRecovery::ReacquireOnDraw { .. });

        let mut queried = false;
        assert!(camera_draw_gate_with_reacquire(true, pending, || {
            queried = true;
            false
        }));
        assert!(
            !queried,
            "an existing current DSV must not be queried again"
        );

        assert!(camera_draw_gate_with_reacquire(false, pending, || true));
        assert!(
            !camera_draw_gate_with_reacquire(false, pending, || false),
            "a UI or other unaudited DSV must stay outside the Draw gate"
        );
        assert!(
            !camera_draw_gate_with_reacquire(false, false, || true),
            "without a phase-reset latch, Draw must not bypass the main DSV gate"
        );
    }

    #[test]
    fn audited_non_1080_dsv_identity_is_selected_without_a_resolution_constant() {
        let expected = CameraDiscoveryRenderIdentity {
            width: 2560,
            height: 1600,
            sample_count: 1,
            formats: vec![DiscoveryDepthFormat::R24G8Typeless],
        };
        let matching = D3D11_TEXTURE2D_DESC {
            Width: 2560,
            Height: 1600,
            Format: DXGI_FORMAT_R24G8_TYPELESS,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            ..Default::default()
        };
        assert!(is_selected_depth_desc(&matching, &expected));

        let old_hard_coded_size = D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1080,
            ..matching
        };
        assert!(!is_selected_depth_desc(&old_hard_coded_size, &expected));
    }

    #[test]
    fn warp_legacy_depth_selection_accepts_real_writable_and_readonly_views() {
        use windows::Win32::Graphics::Direct3D11::{
            D3D11_BIND_SHADER_RESOURCE, D3D11_DEPTH_STENCIL_VIEW_DESC,
            D3D11_DEPTH_STENCIL_VIEW_DESC_0, D3D11_TEX2D_DSV,
        };
        let (device, _context) = warp_device_with_deferred_context();
        let desc = D3D11_TEXTURE2D_DESC {
            Width: 1920,
            Height: 1080,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R24G8_TYPELESS,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_DEPTH_STENCIL.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            ..Default::default()
        };
        let mut texture = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }.unwrap();
        let texture = texture.unwrap();
        let mut views = Vec::new();
        for flags in [0, 3] {
            let desc = D3D11_DEPTH_STENCIL_VIEW_DESC {
                Format: DXGI_FORMAT_D24_UNORM_S8_UINT,
                ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2D,
                Flags: flags,
                Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_DSV { MipSlice: 0 },
                },
            };
            let mut view = None;
            unsafe { device.CreateDepthStencilView(&texture, Some(&desc), Some(&mut view)) }
                .unwrap();
            views.push(view.unwrap());
        }
        let first = super::inspect_dsv_resource(views[0].as_raw()).unwrap();
        let alias = super::inspect_dsv_resource(views[1].as_raw()).unwrap();
        assert_eq!(first.resource_tag, alias.resource_tag);
        assert_ne!(first.view_tag, alias.view_tag);
        let mut selection = legacy_depth_selection();
        for observed in [&first, &alias, &first] {
            assert!(accept_selected_depth_resource(&mut selection, observed).unwrap());
            assert!(selection.has_unique_resource());
        }
        assert!(selection.retired_pairs.is_empty());
    }

    #[test]
    fn legacy_depth_selection_accepts_alias_views_without_retiring_the_texture() {
        let mut selection = legacy_depth_selection();
        let first = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        let mut alias = observed_dsv_with_view(0x1000, 0x1002, 1920, 1080);
        alias.descriptor.view_flags = 3; // Read-only depth/stencil view of the same subresource.
        for observed in [&first, &alias, &first, &alias] {
            assert!(accept_selected_depth_resource(&mut selection, observed).unwrap());
            assert!(selection.has_unique_resource());
            assert!(selection.retired_pairs.is_empty());
        }
        selection.note_explicit_unbind(dsv_pair(0x1000, 0x1002));
        assert!(accept_selected_depth_resource(&mut selection, &first).unwrap());
        assert!(selection.pending_explicit_unbind.is_none());
        assert!(selection.retired_pairs.is_empty());

        let other = observed_dsv_with_view(0x2000, 0x2001, 1920, 1080);
        assert!(accept_selected_depth_resource(&mut selection, &other).is_err());
        assert!(!selection.has_unique_resource());
    }

    #[test]
    fn legacy_depth_selection_retired_resource_cannot_return_through_an_alias() {
        let mut selection = legacy_depth_selection();
        let first = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        let alias = observed_dsv_with_view(0x1000, 0x1002, 1920, 1080);
        let second = observed_dsv_with_view(0x2000, 0x2001, 1920, 1080);
        assert!(accept_selected_depth_resource(&mut selection, &first).unwrap());
        assert!(accept_selected_depth_resource(&mut selection, &alias).unwrap());
        selection.note_explicit_unbind(dsv_pair(0x1000, 0x1002));
        assert!(accept_selected_depth_resource(&mut selection, &second).unwrap());
        selection.note_explicit_unbind(dsv_pair(0x2000, 0x2001));
        assert!(accept_selected_depth_resource(&mut selection, &first).is_err());
        assert!(selection.ambiguous);
    }

    #[test]
    fn legacy_depth_selection_phase_reacquires_same_resource_alias_only() {
        let mut selection = legacy_depth_selection();
        let first = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        let alias = observed_dsv_with_view(0x1000, 0x1002, 1920, 1080);
        assert!(accept_selected_depth_resource(&mut selection, &first).unwrap());
        let expected = selection.expected.clone().unwrap();
        assert_eq!(
            phase_reset_depth_recovery(&selection, Some(dsv_pair(0x1000, 0x1002)), &expected)
                .unwrap(),
            PhaseDepthRecovery::PreserveCurrent {
                pair: dsv_pair(0x1000, 0x1002)
            }
        );
        assert!(
            accept_reacquired_depth_resource(
                &mut selection,
                &alias,
                Some(dsv_pair(0x1000, 0x1001))
            )
            .unwrap()
        );
        let other = observed_dsv_with_view(0x2000, 0x2001, 1920, 1080);
        assert!(
            accept_reacquired_depth_resource(
                &mut selection,
                &other,
                Some(dsv_pair(0x1000, 0x1001))
            )
            .is_err()
        );
        assert!(selection.ambiguous);
    }

    #[test]
    fn exact_depth_selection_still_rejects_two_views_of_one_resource() {
        let first = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        let alias = observed_dsv_with_view(0x1000, 0x1002, 1920, 1080);
        let mut selection = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::ExactDescriptor(first.descriptor)),
            ..Default::default()
        };
        assert!(accept_selected_depth_resource(&mut selection, &first).unwrap());
        assert!(accept_selected_depth_resource(&mut selection, &alias).is_err());
        assert!(selection.ambiguous);
    }

    #[test]
    fn legacy_depth_selection_aliases_share_pass_and_snapshot_identity() {
        let selection = legacy_depth_selection();
        let first = selection.identity(dsv_pair(0x1000, 0x1001));
        let alias = selection.identity(dsv_pair(0x1000, 0x1002));
        assert_eq!(
            first, alias,
            "a view alias must neither reset the pass nor count a snapshot/unbind"
        );
        assert!(
            alias.matches_pair(dsv_pair(0x1000, 0x1001)),
            "alias reacquire must clear the phase latch"
        );
        assert_ne!(first, selection.identity(dsv_pair(0x2000, 0x2001)));
        let exact = DepthSelectionState::default();
        assert_ne!(
            exact.identity(dsv_pair(0x1000, 0x1001)),
            exact.identity(dsv_pair(0x1000, 0x1002))
        );
    }

    #[test]
    fn legacy_depth_selection_does_not_admit_other_subresources_as_aliases() {
        let first = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        for field in 0..4 {
            let mut selection = legacy_depth_selection();
            assert!(accept_selected_depth_resource(&mut selection, &first).unwrap());
            let mut alias = observed_dsv_with_view(0x1000, 0x1002, 1920, 1080);
            match field {
                0 => alias.descriptor.mip_slice = 1,
                1 => alias.descriptor.first_array_slice = 1,
                2 => alias.descriptor.view_array_size = 2,
                _ => alias.descriptor.view_flags = 4,
            }
            assert!(accept_selected_depth_resource(&mut selection, &alias).is_err());
            assert!(selection.ambiguous);
        }
    }

    fn legacy_depth_selection() -> DepthSelectionState {
        DepthSelectionState {
            expected: Some(DepthSelectionExpectation::LegacyRender(
                CameraDiscoveryRenderIdentity {
                    width: 1920,
                    height: 1080,
                    sample_count: 1,
                    formats: vec![DiscoveryDepthFormat::R24G8Typeless],
                },
            )),
            ..Default::default()
        }
    }

    #[test]
    fn two_resources_matching_one_render_identity_are_rejected_for_the_rest_of_the_run() {
        let mut selection = DepthSelectionState::default();
        assert!(!selection.has_unique_resource());
        selection
            .accept_matching_pair(dsv_pair(0x1000, 0x1001))
            .unwrap();
        assert!(selection.has_unique_resource());
        selection
            .accept_matching_pair(dsv_pair(0x1000, 0x1001))
            .unwrap();
        assert!(selection.has_unique_resource());
        assert!(
            selection
                .accept_matching_pair(dsv_pair(0x2000, 0x2001))
                .is_err()
        );
        assert!(!selection.has_unique_resource());
        assert!(
            selection
                .accept_matching_pair(dsv_pair(0x1000, 0x1001))
                .is_err()
        );
    }

    #[test]
    fn exact_descriptor_matches_all_seventeen_fields_and_pair_ambiguity_is_independent() {
        let base = observed_dsv_with_view(0x1000, 0x1001, 1920, 1080);
        let mut mutations = Vec::new();
        macro_rules! mutated {
            ($field:ident, $value:expr) => {{
                let mut descriptor = base.descriptor;
                descriptor.$field = $value;
                mutations.push(descriptor);
            }};
        }
        mutated!(view_format, DXGI_FORMAT_D32_FLOAT.0);
        mutated!(view_dimension, D3D11_DSV_DIMENSION_TEXTURE2DARRAY.0);
        mutated!(view_flags, 1);
        mutated!(mip_slice, 1);
        mutated!(first_array_slice, 1);
        mutated!(view_array_size, 2);
        mutated!(resource_format, DXGI_FORMAT_R32_TYPELESS.0);
        mutated!(width, 1280);
        mutated!(height, 720);
        mutated!(resource_mip_levels, 2);
        mutated!(resource_array_size, 2);
        mutated!(sample_count, 2);
        mutated!(sample_quality, 1);
        mutated!(usage, D3D11_USAGE_STAGING.0);
        mutated!(bind_flags, 0x40);
        mutated!(cpu_access_flags, 1);
        mutated!(misc_flags, 1);
        assert_eq!(mutations.len(), 17);

        for descriptor in mutations {
            let mut selection = DepthSelectionState {
                expected: Some(DepthSelectionExpectation::ExactDescriptor(base.descriptor)),
                ..Default::default()
            };
            let observed = ObservedDsvResource { descriptor, ..base };
            assert!(!accept_selected_depth_resource(&mut selection, &observed).unwrap());
            assert_eq!(selection.selected_pair, None);
        }

        for second in [
            observed_dsv_with_view(0x1000, 0x2001, 1920, 1080),
            observed_dsv_with_view(0x2000, 0x1001, 1920, 1080),
        ] {
            let mut selection = DepthSelectionState {
                expected: Some(DepthSelectionExpectation::ExactDescriptor(base.descriptor)),
                ..Default::default()
            };
            assert!(accept_selected_depth_resource(&mut selection, &base).unwrap());
            assert!(accept_selected_depth_resource(&mut selection, &second).is_err());
            assert!(selection.ambiguous);
        }
    }

    #[test]
    fn retired_pair_ledger_is_bounded_and_overflow_is_fail_closed() {
        let mut selection = DepthSelectionState {
            selected_pair: Some(dsv_pair(1, 2)),
            ..Default::default()
        };
        for index in 0..MAX_RETIRED_DSV_PAIRS {
            selection
                .retired_pairs
                .insert(super::DsvSelectionIdentity::ExactPair(dsv_pair(
                    0x1000 + index as u64,
                    0x2000 + index as u64,
                )));
        }
        selection.note_explicit_unbind(dsv_pair(1, 2));
        assert!(selection.accept_matching_pair(dsv_pair(3, 4)).is_err());
        assert!(selection.ambiguous);
        assert_eq!(selection.retired_pairs.len(), MAX_RETIRED_DSV_PAIRS);
    }

    #[test]
    fn exact_unbound_owner_survives_null_until_rebind_or_serial_replacement() {
        let a = dsv_pair(0x1000, 0x1001);
        let b = dsv_pair(0x2000, 0x2001);
        assert_eq!(
            retained_unbound_transition(Some(a), None),
            RetainedUnboundTransition::RetainOutgoing
        );
        assert_eq!(
            retained_unbound_transition(None, None),
            RetainedUnboundTransition::Keep
        );
        assert_eq!(
            retained_unbound_transition(None, Some(a)),
            RetainedUnboundTransition::Release
        );
        assert_eq!(
            retained_unbound_transition(None, Some(b)),
            RetainedUnboundTransition::Release
        );
        assert_eq!(
            retained_unbound_transition(Some(a), Some(a)),
            RetainedUnboundTransition::Release
        );

        let exact =
            DepthSelectionExpectation::ExactDescriptor(observed_dsv(0x1000, 1920, 1080).descriptor);
        let recovery = PhaseDepthRecovery::ReacquireOnDraw { pair: a };
        assert!(exact_recovery_retained_owner_matches(
            &exact,
            recovery,
            Some(a)
        ));
        assert!(!exact_recovery_retained_owner_matches(
            &exact, recovery, None
        ));
        assert!(!exact_recovery_retained_owner_matches(
            &exact,
            recovery,
            Some(b)
        ));
        assert!(exact_recovery_retained_owner_matches(
            &exact,
            PhaseDepthRecovery::PreserveCurrent { pair: a },
            None
        ));
    }

    #[test]
    fn copy_resource_envelope_rejects_every_unsupported_shape() {
        let valid = observed_dsv(0x1000, 1920, 1080).descriptor;
        validate_d3d11_copy_resource_envelope(valid).unwrap();
        let mut rejected = Vec::new();
        macro_rules! rejected {
            ($field:ident, $value:expr) => {{
                let mut descriptor = valid;
                descriptor.$field = $value;
                rejected.push(descriptor);
            }};
        }
        rejected!(view_format, DXGI_FORMAT_R32_TYPELESS.0);
        rejected!(view_dimension, D3D11_DSV_DIMENSION_TEXTURE2DMS.0);
        rejected!(view_flags, 4);
        rejected!(mip_slice, 1);
        rejected!(first_array_slice, 1);
        rejected!(view_array_size, 2);
        rejected!(resource_format, DXGI_FORMAT_R32_TYPELESS.0);
        rejected!(width, 0);
        rejected!(height, 0);
        rejected!(resource_mip_levels, 2);
        rejected!(resource_array_size, 2);
        rejected!(sample_count, 2);
        rejected!(sample_quality, 1);
        rejected!(usage, D3D11_USAGE_STAGING.0);
        rejected!(bind_flags, D3D11_BIND_CONSTANT_BUFFER.0 as u32);
        rejected!(cpu_access_flags, 1);
        rejected!(misc_flags, 1);
        for descriptor in rejected {
            assert!(
                validate_d3d11_copy_resource_envelope(descriptor).is_err(),
                "unsupported descriptor was admitted: {descriptor:?}"
            );
        }
    }

    #[test]
    fn executable_basename_binding_is_ascii_case_insensitive_but_path_exact() {
        assert!(executable_basename_matches("Game.exe", "game.EXE"));
        assert!(!executable_basename_matches("other.exe", "game.exe"));
        assert!(!executable_basename_matches("dir/game.exe", "game.exe"));
        assert!(!executable_basename_matches("dir\\game.exe", "game.exe"));
        assert!(!executable_basename_matches("游戏.exe", "游戏.exe"));
    }

    #[test]
    fn v3_executable_binding_requires_basename_and_three_way_hash_equality() {
        let trusted = format!("sha256:{}", "a".repeat(64));
        let other = format!("sha256:{}", "b".repeat(64));
        validate_camera_discovery_executable_binding(
            "Game.exe", &trusted, &trusted, "game.EXE", &trusted,
        )
        .unwrap();
        assert!(
            validate_camera_discovery_executable_binding(
                "Other.exe",
                &trusted,
                &trusted,
                "game.exe",
                &trusted,
            )
            .is_err()
        );
        assert!(
            validate_camera_discovery_executable_binding(
                "Game.exe", &trusted, &other, "game.exe", &trusted,
            )
            .is_err(),
            "attestation/config hash drift must be rejected"
        );
        assert!(
            validate_camera_discovery_executable_binding(
                "Game.exe", &trusted, &trusted, "game.exe", &other,
            )
            .is_err(),
            "attestation/fresh actual hash drift must be rejected"
        );
    }

    #[test]
    fn executable_sha256_stream_is_memory_bounded_and_reflects_each_fresh_reader() {
        assert_eq!(
            sha256_stream(std::io::Cursor::new(b"abc")).unwrap(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(
            sha256_stream(std::io::Cursor::new(b"first executable bytes")).unwrap(),
            sha256_stream(std::io::Cursor::new(b"second executable bytes")).unwrap(),
        );
    }

    #[test]
    fn snapshot_integrity_failure_permanently_latches_only_exact_v3_selection() {
        let descriptor = observed_dsv(0x1000, 1920, 1080).descriptor;
        let mut exact = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::ExactDescriptor(descriptor)),
            ..Default::default()
        };
        assert_eq!(
            latch_exact_selection_ambiguity_state(&mut exact),
            Some(true)
        );
        assert!(exact.ambiguous);
        assert_eq!(
            latch_exact_selection_ambiguity_state(&mut exact),
            Some(false)
        );

        let mut legacy = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::LegacyRender(
                CameraDiscoveryRenderIdentity {
                    width: 1920,
                    height: 1080,
                    sample_count: 1,
                    formats: vec![DiscoveryDepthFormat::R24G8Typeless],
                },
            )),
            ..Default::default()
        };
        assert_eq!(latch_exact_selection_ambiguity_state(&mut legacy), None);
        assert!(!legacy.ambiguous);
    }

    #[test]
    fn preinspection_error_and_lock_failures_map_to_exact_fail_closed_policy() {
        let descriptor = observed_dsv(0x1000, 1920, 1080).descriptor;
        let mut exact = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::ExactDescriptor(descriptor)),
            ..Default::default()
        };
        let exact_error: Result<(), String> =
            map_selected_depth_result(Err("synthetic GetResource failure".into()), || {
                let _ = latch_exact_selection_ambiguity_state(&mut exact);
            });
        assert!(exact_error.is_err());
        assert!(exact.ambiguous);

        let mut legacy = DepthSelectionState {
            expected: Some(DepthSelectionExpectation::LegacyRender(
                CameraDiscoveryRenderIdentity {
                    width: 1920,
                    height: 1080,
                    sample_count: 1,
                    formats: vec![DiscoveryDepthFormat::R24G8Typeless],
                },
            )),
            ..Default::default()
        };
        let legacy_error: Result<(), String> = map_selected_depth_result(
            Err("synthetic descriptor inspection failure".into()),
            || {
                let _ = latch_exact_selection_ambiguity_state(&mut legacy);
            },
        );
        assert!(legacy_error.is_err());
        assert!(!legacy.ambiguous);

        assert!(!selected_depth_lock_failure_is_terminal(
            SelectedDepthLockFailure::WouldBlock
        ));
        assert!(selected_depth_lock_failure_is_terminal(
            SelectedDepthLockFailure::Poisoned
        ));
    }

    fn dsv_pair(resource_tag: u64, view_tag: u64) -> DsvPairIdentity {
        DsvPairIdentity {
            resource_tag,
            view_tag,
        }
    }

    fn observed_dsv(resource_tag: u64, width: u32, height: u32) -> ObservedDsvResource {
        observed_dsv_with_view(resource_tag, resource_tag + 1, width, height)
    }

    fn observed_dsv_with_view(
        resource_tag: u64,
        view_tag: u64,
        width: u32,
        height: u32,
    ) -> ObservedDsvResource {
        ObservedDsvResource {
            resource_tag,
            view_tag,
            descriptor: depth_lab::DsvDiscoveryDescriptor {
                view_format: DXGI_FORMAT_D24_UNORM_S8_UINT.0,
                view_dimension: D3D11_DSV_DIMENSION_TEXTURE2D.0,
                view_flags: 0,
                mip_slice: 0,
                first_array_slice: 0,
                view_array_size: 1,
                resource_format: DXGI_FORMAT_R24G8_TYPELESS.0,
                width,
                height,
                resource_mip_levels: 1,
                resource_array_size: 1,
                sample_count: 1,
                sample_quality: 0,
                usage: D3D11_USAGE_DEFAULT.0,
                bind_flags: 0x48,
                cpu_access_flags: 0,
                misc_flags: 0,
            },
        }
    }

    #[test]
    fn production_dsv_tracker_feeds_rank_and_keeps_a_zero_draw_selected_candidate() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let main = observed_dsv(0x1000, 1920, 1080);
        let shadow = observed_dsv(0x2000, 2048, 2048);
        let mut tracker = DsvFrameTracker::default();
        tracker.bind(Some(main));
        tracker.clear(main);
        // Draw 热路径现在免锁攒计数、由下一个持锁入口冲账；这里直接注入等价的 pending。
        tracker.settle_pending(64);
        tracker.bind(Some(shadow));
        tracker.clear(shadow);
        tracker.settle_pending(1);

        let (observations, selected_id) = tracker.finish_frame(Some(main.resource_tag));
        let selected_id = selected_id.unwrap();
        let ranking = depth_lab::dsv::rank(
            &observations,
            depth_lab::Viewport {
                width: 1920,
                height: 1080,
            },
        );
        let audit = depth_lab::DsvSelectionAudit::from_ranking(selected_id, &ranking);
        assert!(audit.selected_matches_top);
        assert!(audit.is_deliverable());

        let (observations, selected_id) = tracker.finish_frame(Some(main.resource_tag));
        let selected_id = selected_id.unwrap();
        assert!(
            observations
                .iter()
                .any(|item| { item.id == selected_id && item.draw_calls == 0 && item.clears == 0 })
        );
    }

    #[test]
    fn pending_draw_settlement_attributes_to_the_active_dsv_at_settle_time() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let main = observed_dsv(0x1000, 1920, 1080);
        let shadow = observed_dsv(0x2000, 2048, 2048);
        let mut tracker = DsvFrameTracker::default();

        // 没有 active DSV 时冲账必须丢弃：这些 draw 不属于任何候选。
        tracker.settle_pending(7);
        // bind 在换 active **之前**冲账，pending 归旧 DSV；这里模拟归属切换。
        tracker.bind(Some(main));
        tracker.settle_pending(3);
        tracker.bind(Some(shadow));
        tracker.settle_pending(2);
        // 解绑（bind None）后攒下的计数没有归属，settle 必须丢弃而不是记给上一个。
        tracker.bind(None);
        tracker.settle_pending(9);

        let (observations, _) = tracker.finish_frame(Some(main.resource_tag));
        let main_obs = observations.iter().find(|o| o.width == 1920).unwrap();
        let shadow_obs = observations.iter().find(|o| o.width == 2048).unwrap();
        assert_eq!(main_obs.draw_calls, 3);
        assert_eq!(shadow_obs.draw_calls, 2);
        assert_eq!(
            observations.iter().map(|o| o.draw_calls).sum::<u64>(),
            5,
            "orphan pending draws must be dropped, not attributed"
        );
    }

    #[test]
    fn discovery_tracker_exposes_exact_descriptors_and_complete_pass_ledger() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let main = observed_dsv(0xAABBCCDD, 2560, 1440);
        let shadow = observed_dsv(0x11223344, 1024, 1024);
        let mut tracker = DsvFrameTracker::default();
        tracker.reset_for_discovery();

        tracker.bind(Some(main));
        tracker.clear(main);
        tracker.settle_pending(17);
        tracker.bind(Some(shadow));
        tracker.clear(shadow);
        tracker.settle_pending(2);

        let frame = tracker.finish_discovery_frame(42);
        assert_eq!(frame.present_seq, 42);
        assert_eq!(frame.candidates.len(), 2);
        let main_row = frame
            .candidates
            .iter()
            .find(|candidate| candidate.descriptor.width == 2560)
            .unwrap();
        assert_eq!(main_row.candidate_id, "dsv-0001");
        assert!(!main_row.candidate_id.contains("AABB"));
        assert_eq!(
            main_row.descriptor.view_format,
            DXGI_FORMAT_D24_UNORM_S8_UINT.0
        );
        assert_eq!(
            main_row.descriptor.resource_format,
            DXGI_FORMAT_R24G8_TYPELESS.0
        );
        assert_eq!(main_row.bind_count, 1);
        assert_eq!(main_row.unbind_count, 1);
        assert_eq!(main_row.pass_count, 1);
        assert_eq!(main_row.draw_calls, 17);
        assert_eq!(main_row.clear_count, 1);
        assert_eq!(main_row.first_bound_order, Some(1));
        assert_eq!(main_row.last_bound_order, Some(1));
        assert!(!main_row.carried_from_previous_present);

        tracker.settle_pending(3);
        let next = tracker.finish_discovery_frame(43);
        let carried = next.candidates.first().unwrap();
        assert_eq!(carried.candidate_id, "dsv-0002");
        assert_eq!(carried.draw_calls, 3);
        assert_eq!(carried.pass_count, 1);
        assert!(carried.carried_from_previous_present);
    }

    #[test]
    fn discovery_tracks_two_views_of_one_resource_as_distinct_candidates() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let first = observed_dsv(0x1000, 1920, 1080);
        let mut second = first;
        second.view_tag = 0x3000;
        let mut tracker = DsvFrameTracker::default();
        tracker.reset_for_discovery();
        tracker.bind(Some(first));
        tracker.bind(Some(second));
        let frame = tracker.finish_discovery_frame(1);
        assert_eq!(
            frame
                .candidates
                .iter()
                .map(|candidate| candidate.candidate_id.as_str())
                .collect::<Vec<_>>(),
            ["dsv-0001", "dsv-0002"]
        );
    }

    #[test]
    fn discovery_pass_ledger_preserves_unbind_order_draws_clears_and_carry() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let main = observed_dsv(0x1000, 1920, 1080);
        let shadow = observed_dsv(0x2000, 2048, 2048);
        let mut tracker = DsvFrameTracker::default();
        tracker.reset_for_discovery();

        tracker.bind(Some(main));
        tracker.clear(main);
        tracker.settle_pending(11);
        tracker.bind(Some(shadow));
        tracker.settle_pending(2);
        tracker.bind(Some(main));
        tracker.settle_pending(23);
        tracker.bind(None);

        let frame = tracker.finish_discovery_frame(1);
        let main_row = frame
            .candidates
            .iter()
            .find(|candidate| candidate.descriptor.width == 1920)
            .unwrap();
        assert_eq!(main_row.unbind_count, 2);
        assert_eq!(
            main_row.completed_passes,
            [
                depth_lab::DsvDiscoveryCompletedPassObservation(11, 1, false),
                depth_lab::DsvDiscoveryCompletedPassObservation(23, 0, false),
            ]
        );

        tracker.bind(Some(main));
        tracker.settle_pending(3);
        let first_half = tracker.finish_discovery_frame(2);
        assert_eq!(first_half.candidates[0].unbind_count, 0);
        assert!(first_half.candidates[0].completed_passes.is_empty());
        tracker.settle_pending(5);
        tracker.bind(None);
        let carried = tracker.finish_discovery_frame(3);
        assert_eq!(
            carried.candidates[0].completed_passes,
            [depth_lab::DsvDiscoveryCompletedPassObservation(8, 0, true)]
        );
    }

    #[test]
    fn discovery_pass_ledger_is_bounded_to_the_formal_snapshot_envelope() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let main = observed_dsv(0x1000, 1920, 1080);
        let shadow = observed_dsv(0x2000, 2048, 2048);
        let mut tracker = DsvFrameTracker::default();
        tracker.reset_for_discovery();
        for _ in 0..13 {
            tracker.bind(Some(main));
            tracker.settle_pending(1);
            tracker.bind(Some(shadow));
        }
        tracker.bind(None);
        let frame = tracker.finish_discovery_frame(1);
        let main_row = frame
            .candidates
            .iter()
            .find(|candidate| candidate.descriptor.width == 1920)
            .unwrap();
        assert_eq!(main_row.unbind_count, 13);
        assert_eq!(
            main_row.completed_passes.len(),
            depth_lab::MAX_DSV_DISCOVERY_COMPLETED_PASSES_PER_CANDIDATE_PER_FRAME
        );
        assert_eq!(main_row.dropped_completed_passes, 1);
    }

    #[test]
    fn production_tracker_keeps_resource_identity_when_dsv_view_descriptor_changes() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let first = observed_dsv(0x1000, 1920, 1080);
        let mut second = first;
        second.view_tag = 0x3000;
        second.descriptor.view_flags = 1;
        let mut tracker = DsvFrameTracker::default();
        tracker.bind(Some(first));
        tracker.bind(Some(second));
        tracker.settle_pending(4);

        let (observations, selected_id) = tracker.finish_frame(Some(first.resource_tag));
        assert_eq!(selected_id.as_deref(), Some("dsv-0001"));
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].draw_calls, 4);
    }

    #[test]
    fn discovery_rejects_view_pointer_reuse_for_another_resource() {
        let _serial = crate::dsv_discovery::lock_test_runtime();
        let first = observed_dsv(0x1000, 1920, 1080);
        let mut reused_view = first;
        reused_view.resource_tag = 0x2000;
        let mut tracker = DsvFrameTracker::default();
        tracker.reset_for_discovery();
        assert!(tracker.register(first));
        assert!(!tracker.register(reused_view));
    }

    #[test]
    fn writer_stop_is_a_fifo_barrier_and_late_frames_cannot_change_files() {
        assert_writer_stop_and_source_identity(None);
    }

    #[test]
    fn d3d12_writer_preserves_selected_profile_identity_and_rejects_drift_before_write() {
        assert_writer_stop_and_source_identity(Some(depth_lab::KCD2_MAIN_SCENE_DSV_ID));
    }

    fn assert_writer_stop_and_source_identity(d3d12_dsv_id: Option<&str>) {
        let root = std::env::temp_dir().join(format!(
            "dm-depth-writer-lifecycle-{}-{}",
            std::process::id(),
            if d3d12_dsv_id.is_some() {
                "d3d12"
            } else {
                "d3d11"
            }
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (tx, rx) = sync_channel(8);
        let worker = std::thread::spawn(move || depth_writer_loop(rx));

        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(DepthWriterCommand::Start {
            generation: 7,
            root: root.clone(),
            projection: IndexProjection::unknown(),
            d3d12_dsv_id: d3d12_dsv_id.map(str::to_owned),
            pass_identity_trace: true,
            reply: reply_tx,
        })
        .unwrap();
        reply_rx.recv().unwrap().unwrap();

        let frame = |present_seq| PendingDepthFrame {
            frame: RawDepthFrame {
                present_seq,
                src_width: 1,
                src_height: 1,
                row_pitch: 4,
                format: DepthFormat::D24S8,
                dst_width: 1,
                dst_height: 1,
                conv: ZConvention::Traditional,
                bytes: vec![0, 0, 128, 0],
                pooled: false,
                verify_pooled: None,
                verify_row_pitch: 0,
            },
            readback_latency_frames: 1,
            generation: 7,
            projection: IndexProjection::unknown(),
            identity_trace: Some(Box::new(PassIdentityTrace {
                schema: PASS_IDENTITY_TRACE_SCHEMA_V1,
                generation: 7,
                present_seq,
                expected_pass_ordinal: Some(0),
                expected_draw_ordinal: Some(6),
                exact_draw_seen: true,
                projection_observed: true,
                selected_unbind_ordinal_from_end: 4,
                snapshots: vec![SnapshotPassIdentityTrace {
                    unbind_ordinal_from_start: 9,
                    unbind_ordinal_from_end: 4,
                    pass_ordinal: Some(0),
                    draw_calls: 3_184,
                    exact_draw_ordinal: Some(6),
                    projection_observed: true,
                }],
                join: PassIdentityJoin::Exact,
            })),
            pass: DsvPassEvidence {
                unbind_ordinal_from_end: 4,
                unbind_total: 12,
                draw_calls: 3_184,
                selection: depth_lab::DsvSelectionAudit {
                    selected_id: d3d12_dsv_id.unwrap_or("dsv-0001").into(),
                    ranked_top_id: Some(d3d12_dsv_id.unwrap_or("dsv-0001").into()),
                    score_margin: Some(42.0),
                    selected_matches_top: true,
                    ranking_confident: true,
                    any_msaa: false,
                    operator_exact_pin: d3d12_dsv_id.is_some(),
                },
            },
        };
        WRITER_QUEUE_DEPTH.store(1, Ordering::Release);
        tx.send(DepthWriterCommand::Frame(frame(10))).unwrap();
        if d3d12_dsv_id.is_some() {
            let mut mismatched = frame(11);
            mismatched.pass.selection.selected_id = "another-dsv".into();
            WRITER_QUEUE_DEPTH.fetch_add(1, Ordering::AcqRel);
            tx.send(DepthWriterCommand::Frame(mismatched)).unwrap();
        }
        let (reply_tx, reply_rx) = sync_channel(1);
        tx.send(DepthWriterCommand::Stop {
            generation: 7,
            reply: reply_tx,
        })
        .unwrap();
        let summary = reply_rx.recv().unwrap().unwrap();
        assert_eq!(summary.row_count, 1);
        assert_eq!(summary.file_count, 1);
        assert_eq!(summary.last_depth_present_seq, Some(10));
        if d3d12_dsv_id.is_some() {
            assert_eq!(summary.errors.len(), 1);
            assert!(summary.errors[0].contains("source identity failed at 11"));
            assert!(!root.join("raw/000000000011.exr").exists());
        } else {
            assert!(summary.errors.is_empty());
        }

        let index_path = root.join("depth-index.jsonl");
        let trace_path = root.join(PASS_IDENTITY_TRACE_FILE);
        let bytes_after_stop = std::fs::read(&index_path).unwrap();
        let index: depth_lab::capture_timing::DepthIndexEntry =
            serde_json::from_slice(&bytes_after_stop).unwrap();
        assert_eq!(index.dsv_id, d3d12_dsv_id.unwrap_or(super::MAIN_DSV_ID));
        assert_eq!(index.schema, depth_lab::capture_timing::INDEX_SCHEMA_V6);
        if d3d12_dsv_id.is_some() {
            assert_eq!(index.dsv_id, index.pass.unwrap().selection.selected_id);
        }
        let trace_after_stop = std::fs::read(&trace_path).unwrap();
        let trace: serde_json::Value = serde_json::from_slice(
            trace_after_stop
                .strip_suffix(b"\n")
                .expect("trace row must be newline terminated"),
        )
        .unwrap();
        assert_eq!(trace["present_seq"], 10);
        assert_eq!(trace["join"], "exact");
        WRITER_QUEUE_DEPTH.store(1, Ordering::Release);
        tx.send(DepthWriterCommand::Frame(frame(11))).unwrap();
        let (barrier_tx, barrier_rx) = sync_channel(1);
        tx.send(DepthWriterCommand::Stop {
            generation: 7,
            reply: barrier_tx,
        })
        .unwrap();
        assert!(barrier_rx.recv().unwrap().is_err());
        assert_eq!(std::fs::read(&index_path).unwrap(), bytes_after_stop);
        assert_eq!(std::fs::read(&trace_path).unwrap(), trace_after_stop);

        drop(tx);
        worker.join().unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}

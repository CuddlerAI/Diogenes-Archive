use std::mem::ManuallyDrop;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use dm_depth_probe::d3d12_backend::{
    D3d12Bootstrap, D3d12ObservationBackend, D3d12ObservationSnapshot,
};
use dm_depth_probe::d3d12_watermark::{D3d12WatermarkRing, WatermarkWriteResult};
use serde::Serialize;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIAdapter1, IDXGIFactory4, IDXGISwapChain1,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DestroyWindow, WS_OVERLAPPEDWINDOW,
};
use windows::core::{Interface, w};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 64;
const FIRST_CLEAR: f32 = 0.25;
const SECOND_CLEAR: f32 = 0.75;
const FENCE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AdapterKind {
    Hardware,
    Warp,
}

#[derive(Debug)]
struct Args {
    adapter_kind: AdapterKind,
    debug_layer: bool,
    iterations: u32,
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct DepthReadbackResult {
    expected: f32,
    min: f32,
    max: f32,
    mismatched_pixels: u64,
    pixels: u64,
}

#[derive(Debug, Serialize)]
struct IterationResult {
    iteration: u32,
    row_pitch_bytes: u32,
    readback_bytes: u64,
    first: DepthReadbackResult,
    second: DepthReadbackResult,
    queue_count: u32,
    command_list_count: u32,
    fence_values: [u64; 2],
    descriptor_overwrite_invalidated_old_identity: bool,
    capture_slots_in_flight_after_iteration: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: &'static str,
    non_trainable: bool,
    adapter_kind: AdapterKind,
    adapter_description: String,
    minimum_feature_level: &'static str,
    debug_layer: bool,
    dimensions: [u32; 2],
    format: &'static str,
    iterations: Vec<IterationResult>,
    hooked_capture: HookedCaptureResult,
    hooked_d32s8_capture: HookedD32S8CaptureResult,
    synthetic_watermark: WatermarkResult,
    early_hook_observation: HookObservationResult,
    factory_swapchain: FactorySwapchainResult,
    passed: bool,
    proven_contracts: [&'static str; 12],
    not_proven: [&'static str; 4],
}

#[derive(Debug, Serialize)]
struct FactorySwapchainResult {
    registered_from_direct_queue: bool,
    pre_present_watermark_submitted: bool,
    resize_cleanup_succeeded: bool,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct HookObservationResult {
    separately_created_device_observed: bool,
    create_command_queue_calls: u64,
    create_command_list_calls: u64,
    create_dsv_calls: u64,
    resource_barrier_calls: u64,
    clear_depth_calls: u64,
    execute_command_lists_calls: u64,
    eligible_pre_clear_snapshots: u64,
    descriptor_overwrites: u64,
    capture_safe: bool,
    snapshot_copy_implemented: bool,
    fence_readback_implemented: bool,
    capture_armed: bool,
    capture_in_flight: usize,
    capture_dropped_pool_full: u64,
    capture_error: Option<String>,
    blockers: Vec<String>,
}

#[derive(Debug, Serialize)]
struct HookedCaptureResult {
    frames: u32,
    first: DepthReadbackResult,
    second: DepthReadbackResult,
    unrelated_direct_queue_ignored: bool,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct HookedD32S8CaptureResult {
    frames: u32,
    first: DepthReadbackResult,
    second: DepthReadbackResult,
    distinct_stencil_values: [u8; 2],
    tight_row_pitch_bytes: usize,
    tight_frame_bytes: usize,
    device_depth_plane_format: i32,
    device_depth_plane_row_bytes: usize,
    device_depth_plane_row_pitch: usize,
    device_depth_plane_element_bytes: usize,
    plane_zero_depth_bit_exact: bool,
    passed: bool,
}

#[derive(Debug, Serialize)]
struct WatermarkResult {
    submitted_sequence: u16,
    decoded_sequence: u16,
    ring_busy_drop_observed: bool,
    incomplete_while_queue_gated: bool,
    present_state_restored_for_copy: bool,
    resize_invalidation_safe_after_fence: bool,
    passed: bool,
}

impl From<D3d12ObservationSnapshot> for HookObservationResult {
    fn from(value: D3d12ObservationSnapshot) -> Self {
        Self {
            separately_created_device_observed: value.create_command_queue_calls > 0,
            create_command_queue_calls: value.create_command_queue_calls,
            create_command_list_calls: value.create_command_list_calls,
            create_dsv_calls: value.create_dsv_calls,
            resource_barrier_calls: value.resource_barrier_calls,
            clear_depth_calls: value.clear_depth_calls,
            execute_command_lists_calls: value.execute_command_lists_calls,
            eligible_pre_clear_snapshots: value.eligible_pre_clear_snapshots,
            descriptor_overwrites: value.descriptor_overwrites,
            capture_safe: value.capture_safe,
            snapshot_copy_implemented: value.snapshot_copy_implemented,
            fence_readback_implemented: value.fence_readback_implemented,
            capture_armed: value.capture_armed,
            capture_in_flight: value.capture_in_flight,
            capture_dropped_pool_full: value.capture_dropped_pool_full,
            capture_error: value.capture_error,
            blockers: value.blockers,
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = parse_args()?;
    if args.debug_layer {
        enable_diagnostics()?;
    }

    // Arm device hooks on an independently created dummy device first. The
    // actual smoke device below must then hit the same shared runtime vtable.
    let bootstrap = D3d12Bootstrap::default_adapter()
        .map_err(anyhow::Error::msg)
        .context("arming early D3D12 observation hooks failed")?;
    let (adapter, adapter_description) = select_adapter(args.adapter_kind)?;
    let device = create_device(&adapter)?;
    let factory_swapchain = run_factory_swapchain_smoke(&device, bootstrap.backend())?;
    let hooked_capture = run_hooked_capture_smoke(&device, bootstrap.backend())?;
    bootstrap
        .backend()
        .begin_resource_epoch()
        .map_err(anyhow::Error::msg)
        .context("advancing observation epoch after hooked capture smoke failed")?;
    let hooked_d32s8_capture = run_hooked_d32s8_capture_smoke(&device, bootstrap.backend())?;
    bootstrap
        .backend()
        .begin_resource_epoch()
        .map_err(anyhow::Error::msg)
        .context("advancing observation epoch after hooked D32S8 capture smoke failed")?;
    let synthetic_watermark = run_watermark_smoke(&device)?;
    bootstrap
        .backend()
        .begin_resource_epoch()
        .map_err(anyhow::Error::msg)
        .context("advancing observation epoch after watermark smoke failed")?;
    let mut results = Vec::with_capacity(args.iterations as usize);
    for iteration in 1..=args.iterations {
        let result = run_iteration(&device, bootstrap.backend(), iteration)
            .with_context(|| format!("D3D12 depth smoke iteration {iteration} failed"))?;
        results.push(result);
        if iteration < args.iterations {
            bootstrap
                .backend()
                .begin_resource_epoch()
                .map_err(anyhow::Error::msg)
                .with_context(|| {
                    format!("D3D12 observation epoch advance after iteration {iteration} failed")
                })?;
        }
    }

    let hook_snapshot = bootstrap.backend().snapshot();
    let expected_queue_calls = u64::from(args.iterations) * 2;
    let expected_list_calls = u64::from(args.iterations) * 2;
    let hook_observation_passed = hook_snapshot.create_command_queue_calls >= expected_queue_calls
        && hook_snapshot.create_command_list_calls >= expected_list_calls
        && hook_snapshot.create_dsv_calls >= u64::from(args.iterations) * 2
        && hook_snapshot.execute_command_lists_calls >= u64::from(args.iterations) * 2
        && hook_snapshot.eligible_pre_clear_snapshots >= u64::from(args.iterations) * 2
        && hook_snapshot.capture_safe
        && hook_snapshot.snapshot_copy_implemented
        && hook_snapshot.fence_readback_implemented
        && !hook_snapshot.capture_armed
        && hook_snapshot.capture_in_flight == 0
        && hook_snapshot.capture_error.is_none();
    let passed = hooked_capture.passed
        && hooked_d32s8_capture.passed
        && synthetic_watermark.passed
        && factory_swapchain.passed
        && hook_observation_passed
        && results.iter().all(|result| {
            result.first.mismatched_pixels == 0
                && result.second.mismatched_pixels == 0
                && result.queue_count == 2
                && result.command_list_count == 2
                && result.descriptor_overwrite_invalidated_old_identity
                && result.capture_slots_in_flight_after_iteration == 0
        });
    let report = Report {
        schema_version: "dm-d3d12-depth-smoke-2",
        non_trainable: true,
        adapter_kind: args.adapter_kind,
        adapter_description,
        minimum_feature_level: "12_0",
        debug_layer: args.debug_layer,
        dimensions: [WIDTH, HEIGHT],
        format: "D32_FLOAT",
        iterations: results,
        hooked_capture,
        hooked_d32s8_capture,
        synthetic_watermark,
        early_hook_observation: hook_snapshot.into(),
        factory_swapchain,
        passed,
        proven_contracts: [
            "d3d12_device_and_direct_queue_creation",
            "dsv_descriptor_creation_and_depth_clear",
            "depth_write_copy_source_depth_write_transitions",
            "texture_to_readback_copy_with_copyable_footprint",
            "queue_fence_completion_before_cpu_map",
            "hooked_descriptor_epoch_and_capture_slot_fail_closed_model",
            "multiple_direct_queue_and_command_list_ordering",
            "dsv_descriptor_overwrite_invalidates_old_identity",
            "dummy_device_hooks_observe_separately_created_device",
            "hooked_outgoing_depth_write_exit_copy_and_nonblocking_fence_drain",
            "d32s8_device_plane_zero_copy_to_tight_d32_without_stencil",
            "synthetic_present_render_target_present_watermark_restore",
        ],
        not_proven: [
            "swapchain_present_watermark_ordering",
            "injected_vtable_hook_safety",
            "game_dsv_and_exact_draw_identity",
            "cp2077_runtime_compatibility",
        ],
    };

    let json = serde_json::to_string_pretty(&report)? + "\n";
    print!("{json}");
    if let Some(path) = args.output {
        if path.exists() {
            anyhow::bail!("refusing to overwrite report: {}", path.display());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, json)
            .with_context(|| format!("failed to write {}", path.display()))?;
        eprintln!("report={}", path.display());
    }
    ensure!(report.passed, "D3D12 depth smoke verification failed");
    Ok(())
}

fn parse_args() -> anyhow::Result<Args> {
    let mut adapter_kind = AdapterKind::Hardware;
    let mut debug_layer = false;
    let mut iterations = 1_u32;
    let mut output = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--warp" => adapter_kind = AdapterKind::Warp,
            "--debug" => debug_layer = true,
            "--iterations" => {
                iterations = args
                    .next()
                    .context("--iterations requires a number")?
                    .parse()
                    .context("--iterations must be an integer")?;
            }
            "--out" => {
                output = Some(
                    args.next()
                        .map(PathBuf::from)
                        .context("--out requires a path")?,
                );
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    ensure!(
        (1..=1_000).contains(&iterations),
        "--iterations must be within 1..=1000"
    );
    Ok(Args {
        adapter_kind,
        debug_layer,
        iterations,
        output,
    })
}

fn enable_diagnostics() -> anyhow::Result<()> {
    let mut debug = None;
    unsafe { D3D12GetDebugInterface::<ID3D12Debug>(&mut debug) }
        .context("D3D12 debug layer is unavailable; install Windows Graphics Tools")?;
    unsafe {
        debug
            .context("D3D12GetDebugInterface returned no debug interface")?
            .EnableDebugLayer();
    }

    let mut dred = None;
    unsafe { D3D12GetDebugInterface::<ID3D12DeviceRemovedExtendedDataSettings>(&mut dred) }
        .context("DRED settings interface is unavailable")?;
    let dred = dred.context("D3D12GetDebugInterface returned no DRED settings")?;
    unsafe {
        dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
        dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
    }
    Ok(())
}

fn select_adapter(kind: AdapterKind) -> anyhow::Result<(IDXGIAdapter1, String)> {
    let factory: IDXGIFactory4 =
        unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1 failed")?;
    if kind == AdapterKind::Warp {
        let adapter: IDXGIAdapter1 = unsafe { factory.EnumWarpAdapter() }
            .context("IDXGIFactory4::EnumWarpAdapter failed")?;
        let description = adapter_description(&adapter)?;
        return Ok((adapter, description));
    }

    let mut index = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error).context("DXGI adapter enumeration failed"),
        };
        let desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1 failed")?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 && create_device(&adapter).is_ok()
        {
            return Ok((adapter, utf16_z(&desc.Description)));
        }
        index += 1;
    }
    anyhow::bail!("no hardware adapter supports D3D12 feature level 12_0")
}

fn adapter_description(adapter: &IDXGIAdapter1) -> anyhow::Result<String> {
    let desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1 failed")?;
    Ok(utf16_z(&desc.Description))
}

fn utf16_z(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn create_device(adapter: &IDXGIAdapter1) -> anyhow::Result<ID3D12Device> {
    let mut device = None;
    unsafe { D3D12CreateDevice(adapter, D3D_FEATURE_LEVEL_12_0, &mut device) }
        .context("D3D12CreateDevice(feature level 12_0) failed")?;
    device.context("D3D12CreateDevice returned no device")
}

fn run_factory_swapchain_smoke(
    device: &ID3D12Device,
    backend: &std::sync::Arc<D3d12ObservationBackend>,
) -> anyhow::Result<FactorySwapchainResult> {
    let hwnd: HWND = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("dm-d3d12-factory-smoke"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            320,
            240,
            None,
            None,
            None,
            None,
        )
    }
    .context("creating hidden DXGI smoke window failed")?;
    let result = (|| -> anyhow::Result<FactorySwapchainResult> {
        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
            Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
            NodeMask: 0,
        };
        let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
            .context("factory smoke DIRECT queue creation failed")?;
        let factory: IDXGIFactory4 =
            unsafe { CreateDXGIFactory1() }.context("factory smoke CreateDXGIFactory1 failed")?;
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: 320,
            Height: 240,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            AlphaMode: Default::default(),
            Flags: 0,
        };
        let swapchain: IDXGISwapChain1 =
            unsafe { factory.CreateSwapChainForHwnd(&queue, hwnd, &desc, None, None) }
                .context("factory smoke CreateSwapChainForHwnd failed")?;
        let raw = swapchain.as_raw();
        let registered_from_direct_queue = backend.is_d3d12_swapchain(raw);
        ensure!(
            registered_from_direct_queue,
            "separately-created factory swapchain did not hit the shared factory hook"
        );
        let present = backend
            .before_present(raw, 7, true, 0, 0)
            .map_err(anyhow::Error::msg)
            .context("factory swapchain pre-Present watermark failed")?;
        unsafe { swapchain.Present(0, DXGI_PRESENT(0)) }
            .ok()
            .context("factory smoke Present failed")?;
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .context("factory smoke fence creation failed")?;
        unsafe { queue.Signal(&fence, 1) }.context("factory smoke fence signal failed")?;
        wait_for_fence(device, &fence, 1)?;
        backend
            .prepare_swapchain_resize(raw)
            .map_err(anyhow::Error::msg)
            .context("factory swapchain resize cleanup failed")?;
        backend.unregister_swapchain(raw);
        Ok(FactorySwapchainResult {
            registered_from_direct_queue,
            pre_present_watermark_submitted: present.watermark_submitted,
            resize_cleanup_succeeded: true,
            passed: registered_from_direct_queue && present.watermark_submitted,
        })
    })();
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    result
}

fn run_hooked_capture_smoke(
    device: &ID3D12Device,
    backend: &std::sync::Arc<D3d12ObservationBackend>,
) -> anyhow::Result<HookedCaptureResult> {
    let queue_desc = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .context("hooked capture queue creation failed")?;
    let depth_desc = depth_resource_desc();
    let clear_value = D3D12_CLEAR_VALUE {
        Format: DXGI_FORMAT_D32_FLOAT,
        Anonymous: D3D12_CLEAR_VALUE_0 {
            DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                Depth: FIRST_CLEAR,
                Stencil: 0,
            },
        },
    };
    let mut depth = None;
    unsafe {
        device.CreateCommittedResource(
            &heap_properties(D3D12_HEAP_TYPE_DEFAULT),
            D3D12_HEAP_FLAG_NONE,
            &depth_desc,
            D3D12_RESOURCE_STATE_COMMON,
            Some(&clear_value),
            &mut depth,
        )
    }
    .context("hooked capture depth resource creation failed")?;
    let depth: ID3D12Resource = depth.context("hooked capture depth resource is null")?;
    let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
        NumDescriptors: 1,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        NodeMask: 0,
    };
    let heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&heap_desc) }
        .context("hooked capture DSV heap creation failed")?;
    let dsv = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
    unsafe { device.CreateDepthStencilView(&depth, None, dsv) };

    let seed_allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("hooked capture seed allocator creation failed")?;
    let seed_list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &seed_allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("hooked capture seed list creation failed")?;
    record_transition(
        &seed_list,
        &depth,
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    unsafe {
        seed_list.OMSetRenderTargets(0, None, false, Some(&dsv));
        seed_list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, FIRST_CLEAR, 0, None);
        seed_list.Close()
    }
    .context("hooked capture seed list close failed")?;
    let seed_base: ID3D12CommandList = seed_list.cast()?;
    unsafe { queue.ExecuteCommandLists(&[Some(seed_base)]) };
    let seed_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("hooked capture seed fence creation failed")?;
    unsafe { queue.Signal(&seed_fence, 1) }.context("hooked capture seed signal failed")?;
    wait_for_fence(device, &seed_fence, 1)?;

    backend
        .arm_depth_capture_by_handle(dsv.ptr as u64, 4, None, None)
        .map_err(anyhow::Error::msg)
        .context("arming hooked D3D12 capture failed")?;
    let allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("hooked capture allocator creation failed")?;
    let list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("hooked capture list creation failed")?;
    // Re-establish the exact command-list-local DEPTH_WRITE proof used by the
    // discovery auditor. This COPY_SOURCE round trip is not itself an admitted
    // capture transition; only the later CP2077 0xe0/0x40 exits trigger copy.
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    backend.begin_depth_capture_interval(40, true);
    unsafe {
        list.OMSetRenderTargets(0, None, false, Some(&dsv));
        list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, SECOND_CLEAR, 0, None);
    }
    // The selected pass has completed. Capture must happen before the original
    // full-resource DEPTH_WRITE -> composed-read barrier, not at the following
    // frame's clear.
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATES(0xe0),
    );
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATES(0xe0),
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    backend.begin_depth_capture_interval(41, true);
    unsafe {
        list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, FIRST_CLEAR, 0, None);
    }
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATES(0x40),
    );
    unsafe { list.Close() }.context("hooked capture list close failed")?;
    let base: ID3D12CommandList = list.cast()?;
    unsafe { queue.ExecuteCommandLists(&[Some(base)]) };
    let game_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("hooked capture game fence creation failed")?;
    unsafe { queue.Signal(&game_fence, 1) }.context("hooked capture game signal failed")?;
    wait_for_fence(device, &game_fence, 1)?;

    // A second DIRECT queue that submits no captured list must not be mistaken
    // for queue drift. Queue identity is latched only to the list carrying the
    // injected copy.
    let unrelated_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .context("unrelated capture queue creation failed")?;
    let unrelated_allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("unrelated capture allocator creation failed")?;
    let unrelated_list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &unrelated_allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("unrelated capture list creation failed")?;
    unsafe { unrelated_list.Close() }.context("unrelated capture list close failed")?;
    let unrelated_base: ID3D12CommandList = unrelated_list.cast()?;
    unsafe { unrelated_queue.ExecuteCommandLists(&[Some(unrelated_base)]) };
    let unrelated_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("unrelated capture fence creation failed")?;
    unsafe { unrelated_queue.Signal(&unrelated_fence, 1) }
        .context("unrelated capture signal failed")?;
    wait_for_fence(device, &unrelated_fence, 1)?;

    let frames = backend
        .drain_depth_capture()
        .map_err(anyhow::Error::msg)
        .context("draining hooked D3D12 capture failed")?;
    ensure!(
        frames.len() == 2,
        "hooked capture returned {} frames",
        frames.len()
    );
    ensure!(
        frames[0].present_seq == 40 && frames[1].present_seq == 41,
        "hooked capture interval labels drifted: {:?}",
        frames
            .iter()
            .map(|frame| frame.present_seq)
            .collect::<Vec<_>>()
    );
    let first = inspect_raw_depth(
        &frames[0].bytes,
        frames[0].row_pitch,
        frames[0].src_width,
        frames[0].src_height,
        SECOND_CLEAR,
    )?;
    let second = inspect_raw_depth(
        &frames[1].bytes,
        frames[1].row_pitch,
        frames[1].src_width,
        frames[1].src_height,
        FIRST_CLEAR,
    )?;
    let status = backend.snapshot();
    let unrelated_direct_queue_ignored = status.capture_error.is_none();
    let passed = first.mismatched_pixels == 0
        && second.mismatched_pixels == 0
        && unrelated_direct_queue_ignored
        && status.capture_dropped_pool_full == 0
        && status.capture_in_flight == 0;
    backend
        .disarm_depth_capture()
        .map_err(anyhow::Error::msg)
        .context("disarming hooked D3D12 capture failed")?;
    Ok(HookedCaptureResult {
        frames: frames.len() as u32,
        first,
        second,
        unrelated_direct_queue_ignored,
        passed,
    })
}

fn run_hooked_d32s8_capture_smoke(
    device: &ID3D12Device,
    backend: &Arc<D3d12ObservationBackend>,
) -> anyhow::Result<HookedD32S8CaptureResult> {
    const FIRST_DEPTH: f32 = 0.3125;
    const SECOND_DEPTH: f32 = 0.8125;
    const FIRST_STENCIL: u8 = 0x2d;
    const SECOND_STENCIL: u8 = 0xb7;
    let queue_desc = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .context("D32S8 capture queue creation failed")?;
    let depth_desc = D3D12_RESOURCE_DESC {
        Format: DXGI_FORMAT_R32G8X24_TYPELESS,
        ..depth_resource_desc()
    };
    let clear_value = D3D12_CLEAR_VALUE {
        Format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        Anonymous: D3D12_CLEAR_VALUE_0 {
            DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                Depth: FIRST_DEPTH,
                Stencil: FIRST_STENCIL,
            },
        },
    };
    let mut depth = None;
    unsafe {
        device.CreateCommittedResource(
            &heap_properties(D3D12_HEAP_TYPE_DEFAULT),
            D3D12_HEAP_FLAG_NONE,
            &depth_desc,
            D3D12_RESOURCE_STATE_COMMON,
            Some(&clear_value),
            &mut depth,
        )
    }
    .context("D32S8 capture depth resource creation failed")?;
    let depth: ID3D12Resource = depth.context("D32S8 capture depth resource is null")?;
    let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
        NumDescriptors: 1,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        NodeMask: 0,
    };
    let heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&heap_desc) }
        .context("D32S8 capture DSV heap creation failed")?;
    let dsv = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
    let dsv_desc = D3D12_DEPTH_STENCIL_VIEW_DESC {
        Format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
        ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
        ..Default::default()
    };
    unsafe { device.CreateDepthStencilView(&depth, Some(&dsv_desc), dsv) };

    let allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("D32S8 capture allocator creation failed")?;
    let list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("D32S8 capture list creation failed")?;
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );

    backend
        .arm_depth_capture_by_handle(dsv.ptr as u64, 4, None, None)
        .map_err(anyhow::Error::msg)
        .context("arming hooked D32S8 capture failed")?;
    backend.begin_depth_capture_interval(50, true);
    unsafe {
        list.OMSetRenderTargets(0, None, false, Some(&dsv));
        list.ClearDepthStencilView(
            dsv,
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            FIRST_DEPTH,
            FIRST_STENCIL,
            None,
        );
    }
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATES(0xe0),
    );
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATES(0xe0),
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    backend.begin_depth_capture_interval(51, true);
    unsafe {
        list.ClearDepthStencilView(
            dsv,
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            SECOND_DEPTH,
            SECOND_STENCIL,
            None,
        );
    }
    record_transition(
        &list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATES(0x40),
    );
    unsafe { list.Close() }.context("D32S8 capture list close failed")?;
    let base: ID3D12CommandList = list.cast()?;
    unsafe { queue.ExecuteCommandLists(&[Some(base)]) };
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("D32S8 capture fence creation failed")?;
    unsafe { queue.Signal(&fence, 1) }.context("D32S8 capture fence signal failed")?;
    wait_for_fence(device, &fence, 1)?;

    let frames = backend
        .drain_depth_capture()
        .map_err(anyhow::Error::msg)
        .context("draining hooked D32S8 capture failed")?;
    ensure!(
        frames.len() == 2,
        "D32S8 capture returned {} frames",
        frames.len()
    );
    ensure!(
        frames[0].present_seq == 50 && frames[1].present_seq == 51,
        "D32S8 interval labels drifted"
    );
    for frame in &frames {
        ensure!(
            frame.source_plane_element_bytes == 4 || frame.source_plane_element_bytes == 8,
            "D32S8 plane-zero element size is neither device-described R32 nor R32X32: {}",
            frame.source_plane_element_bytes,
        );
        ensure!(
            frame.source_plane_row_bytes == WIDTH as usize * frame.source_plane_element_bytes,
            "D32S8 plane-zero row bytes disagree with device footprint: {}",
            frame.source_plane_row_bytes
        );
        ensure!(
            frame.row_pitch == WIDTH as usize * 4
                && frame.bytes.len() == WIDTH as usize * HEIGHT as usize * 4,
            "D32S8 output is not tight D32"
        );
    }
    let first = inspect_raw_depth(
        &frames[0].bytes,
        frames[0].row_pitch,
        frames[0].src_width,
        frames[0].src_height,
        FIRST_DEPTH,
    )?;
    let second = inspect_raw_depth(
        &frames[1].bytes,
        frames[1].row_pitch,
        frames[1].src_width,
        frames[1].src_height,
        SECOND_DEPTH,
    )?;
    let plane_zero_depth_bit_exact = first.mismatched_pixels == 0 && second.mismatched_pixels == 0;
    let tight_row_pitch_bytes = frames[0].row_pitch;
    let tight_frame_bytes = frames[0].bytes.len();
    let device_depth_plane_format = frames[0].source_plane_format;
    let device_depth_plane_row_bytes = frames[0].source_plane_row_bytes;
    let device_depth_plane_row_pitch = frames[0].source_plane_row_pitch;
    let device_depth_plane_element_bytes = frames[0].source_plane_element_bytes;
    let status = backend.snapshot();
    let passed = plane_zero_depth_bit_exact
        && status.capture_error.is_none()
        && status.capture_in_flight == 0;
    backend
        .disarm_depth_capture()
        .map_err(anyhow::Error::msg)
        .context("disarming hooked D32S8 capture failed")?;
    Ok(HookedD32S8CaptureResult {
        frames: frames.len() as u32,
        first,
        second,
        distinct_stencil_values: [FIRST_STENCIL, SECOND_STENCIL],
        tight_row_pitch_bytes,
        tight_frame_bytes,
        device_depth_plane_format,
        device_depth_plane_row_bytes,
        device_depth_plane_row_pitch,
        device_depth_plane_element_bytes,
        plane_zero_depth_bit_exact,
        passed,
    })
}

fn inspect_raw_depth(
    bytes: &[u8],
    row_pitch: usize,
    width: usize,
    height: usize,
    expected: f32,
) -> anyhow::Result<DepthReadbackResult> {
    ensure!(row_pitch >= width * 4, "raw D32 row pitch is too small");
    ensure!(
        bytes.len() >= row_pitch * height,
        "raw D32 buffer is shorter than its footprint"
    );
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut mismatched_pixels = 0_u64;
    for y in 0..height {
        for x in 0..width {
            let offset = y * row_pitch + x * 4;
            let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            min = min.min(value);
            max = max.max(value);
            if value.to_bits() != expected.to_bits() {
                mismatched_pixels += 1;
            }
        }
    }
    Ok(DepthReadbackResult {
        expected,
        min,
        max,
        mismatched_pixels,
        pixels: (width * height) as u64,
    })
}

fn run_watermark_smoke(device: &ID3D12Device) -> anyhow::Result<WatermarkResult> {
    const SEQUENCES: [u16; 3] = [0x1234, 0x5678, 0xabcd];
    let queue_desc = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .context("watermark queue creation failed")?;
    let target_desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: u64::from(depth_lab::watermark::RECT_W),
        Height: depth_lab::watermark::RECT_H,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    };
    let mut target = None;
    unsafe {
        device.CreateCommittedResource(
            &heap_properties(D3D12_HEAP_TYPE_DEFAULT),
            D3D12_HEAP_FLAG_NONE,
            &target_desc,
            D3D12_RESOURCE_STATE_PRESENT,
            None,
            &mut target,
        )
    }
    .context("watermark target creation failed")?;
    let target: ID3D12Resource = target.context("watermark target is null")?;

    // Hold the GPU queue behind an unsignaled fence. Three private slots can
    // still be submitted without a CPU wait, while a fourth write must be
    // rejected immediately as ring-busy.
    let gate: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("watermark gate fence creation failed")?;
    unsafe { queue.Wait(&gate, 1) }.context("watermark queue gate failed")?;
    let mut ring = D3d12WatermarkRing::new(device, &queue, std::slice::from_ref(&target), 3)
        .map_err(anyhow::Error::msg)
        .context("watermark ring creation failed")?;
    let mut final_ticket = None;
    for sequence in SEQUENCES {
        let result = ring
            .write(0, sequence, 0, 0)
            .map_err(anyhow::Error::msg)
            .context("watermark submission failed")?;
        match result {
            WatermarkWriteResult::Submitted(ticket) => final_ticket = Some(ticket),
            WatermarkWriteResult::DroppedRingBusy => {
                anyhow::bail!("watermark ring dropped before all private slots were used")
            }
        }
    }
    let final_ticket = final_ticket.context("watermark did not produce a ticket")?;
    let incomplete_while_queue_gated = !ring.is_complete(final_ticket);
    let ring_busy_drop_observed = matches!(
        ring.write(0, 0xffff, 0, 0).map_err(anyhow::Error::msg)?,
        WatermarkWriteResult::DroppedRingBusy
    ) && ring.dropped_busy() == 1;
    unsafe { gate.Signal(1) }.context("releasing watermark queue gate failed")?;
    let started = Instant::now();
    while !ring.is_complete(final_ticket) {
        ensure!(
            started.elapsed() < FENCE_TIMEOUT,
            "watermark private fence did not complete within {FENCE_TIMEOUT:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let resize_invalidation_safe_after_fence = ring.can_invalidate();

    // A PRESENT -> COPY_SOURCE -> PRESENT submission after the watermark is a
    // functional check that the private list restored the advertised state.
    let (readback, footprint, total_bytes) = create_readback(device, &target_desc)?;
    let allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("watermark readback allocator creation failed")?;
    let list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("watermark readback list creation failed")?;
    record_transition(
        &list,
        &target,
        D3D12_RESOURCE_STATE_PRESENT,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    record_copy(&list, &target, &readback, footprint);
    record_transition(
        &list,
        &target,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_PRESENT,
    );
    unsafe { list.Close() }.context("watermark readback list close failed")?;
    let base: ID3D12CommandList = list.cast()?;
    unsafe { queue.ExecuteCommandLists(&[Some(base)]) };
    let done: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("watermark readback fence creation failed")?;
    unsafe { queue.Signal(&done, 1) }.context("watermark readback signal failed")?;
    wait_for_fence(device, &done, 1)?;
    let decoded_sequence = inspect_watermark(&readback, footprint.Footprint.RowPitch, total_bytes)?;
    let present_state_restored_for_copy = decoded_sequence == SEQUENCES[2];
    let passed = decoded_sequence == SEQUENCES[2]
        && ring_busy_drop_observed
        && incomplete_while_queue_gated
        && present_state_restored_for_copy
        && resize_invalidation_safe_after_fence
        && ring.fatal_error().is_none();
    Ok(WatermarkResult {
        submitted_sequence: SEQUENCES[2],
        decoded_sequence,
        ring_busy_drop_observed,
        incomplete_while_queue_gated,
        present_state_restored_for_copy,
        resize_invalidation_safe_after_fence,
        passed,
    })
}

fn inspect_watermark(
    resource: &ID3D12Resource,
    row_pitch: u32,
    total_bytes: u64,
) -> anyhow::Result<u16> {
    let read_range = D3D12_RANGE {
        Begin: 0,
        End: usize::try_from(total_bytes).context("watermark readback size exceeds usize")?,
    };
    let mut mapped = std::ptr::null_mut();
    unsafe { resource.Map(0, Some(&read_range), Some(&mut mapped)) }
        .context("mapping watermark readback failed")?;
    ensure!(!mapped.is_null(), "watermark readback Map returned null");
    let bytes = unsafe {
        std::slice::from_raw_parts(
            mapped.cast::<u8>(),
            usize::try_from(total_bytes).context("watermark readback size exceeds usize")?,
        )
    };
    let means = depth_lab::watermark::block_means_from_bgra(bytes, row_pitch as usize)
        .context("sampling watermark blocks failed")?;
    let sequence = depth_lab::watermark::decode(&means, 32)
        .context("decoding synthetic D3D12 watermark failed")?;
    let no_write = D3D12_RANGE { Begin: 0, End: 0 };
    unsafe { resource.Unmap(0, Some(&no_write)) };
    Ok(sequence)
}

fn run_iteration(
    device: &ID3D12Device,
    backend: &Arc<D3d12ObservationBackend>,
    iteration: u32,
) -> anyhow::Result<IterationResult> {
    let queue_desc = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        Priority: D3D12_COMMAND_QUEUE_PRIORITY_NORMAL.0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .context("ID3D12Device::CreateCommandQueue failed")?;
    let secondary_queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .context("creating the secondary ID3D12CommandQueue failed")?;
    let allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("ID3D12Device::CreateCommandAllocator failed")?;
    let command_list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("ID3D12Device::CreateCommandList failed")?;
    let secondary_allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .context("creating the secondary ID3D12CommandAllocator failed")?;
    let secondary_command_list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &secondary_allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .context("creating the secondary ID3D12GraphicsCommandList failed")?;

    let depth_desc = depth_resource_desc();
    let clear_value = D3D12_CLEAR_VALUE {
        Format: DXGI_FORMAT_D32_FLOAT,
        Anonymous: D3D12_CLEAR_VALUE_0 {
            DepthStencil: D3D12_DEPTH_STENCIL_VALUE {
                Depth: FIRST_CLEAR,
                Stencil: 0,
            },
        },
    };
    let default_heap = heap_properties(D3D12_HEAP_TYPE_DEFAULT);
    let mut depth = None;
    unsafe {
        device.CreateCommittedResource(
            &default_heap,
            D3D12_HEAP_FLAG_NONE,
            &depth_desc,
            D3D12_RESOURCE_STATE_COMMON,
            Some(&clear_value),
            &mut depth,
        )
    }
    .context("creating the D32 depth resource failed")?;
    let depth: ID3D12Resource =
        depth.context("CreateCommittedResource returned no depth resource")?;

    let dsv_heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
        NumDescriptors: 1,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        NodeMask: 0,
    };
    let dsv_heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&dsv_heap_desc) }
        .context("creating the DSV descriptor heap failed")?;
    let dsv = unsafe { dsv_heap.GetCPUDescriptorHandleForHeapStart() };
    unsafe { device.CreateDepthStencilView(&depth, None, dsv) };

    let (first_readback, footprint, total_bytes) = create_readback(device, &depth_desc)?;
    let (second_readback, second_footprint, second_total_bytes) =
        create_readback(device, &depth_desc)?;
    ensure!(
        footprint == second_footprint && total_bytes == second_total_bytes,
        "identical D32 resources produced different copyable footprints"
    );

    let fence_value = 1;
    let secondary_fence_value = 1;

    record_transition(
        &command_list,
        &depth,
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    unsafe {
        command_list.OMSetRenderTargets(0, None, false, Some(&dsv));
        command_list.ClearDepthStencilView(dsv, D3D12_CLEAR_FLAG_DEPTH, FIRST_CLEAR, 0, None);
    }
    record_transition(
        &command_list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    record_copy(&command_list, &depth, &first_readback, footprint);
    record_transition(
        &command_list,
        &depth,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );

    // The observation contract deliberately requires same-command-list proof
    // of DEPTH_WRITE rather than inferring state across queues.  Express an
    // identity transition pair on the secondary list before its clear; the
    // primary queue leaves the resource in DEPTH_WRITE and the queue Wait below
    // establishes execution order.
    record_transition(
        &secondary_command_list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    record_transition(
        &secondary_command_list,
        &depth,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );
    unsafe {
        secondary_command_list.OMSetRenderTargets(0, None, false, Some(&dsv));
        secondary_command_list.ClearDepthStencilView(
            dsv,
            D3D12_CLEAR_FLAG_DEPTH,
            SECOND_CLEAR,
            0,
            None,
        );
    }
    record_transition(
        &secondary_command_list,
        &depth,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
    );
    record_copy(
        &secondary_command_list,
        &depth,
        &second_readback,
        second_footprint,
    );
    record_transition(
        &secondary_command_list,
        &depth,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_DEPTH_WRITE,
    );

    unsafe { command_list.Close() }.context("closing the D3D12 command list failed")?;
    unsafe { secondary_command_list.Close() }
        .context("closing the secondary D3D12 command list failed")?;
    let base_list: ID3D12CommandList = command_list.cast()?;
    unsafe { queue.ExecuteCommandLists(&[Some(base_list)]) };
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("ID3D12Device::CreateFence failed")?;
    unsafe { queue.Signal(&fence, fence_value) }.context("ID3D12CommandQueue::Signal failed")?;
    let secondary_fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .context("creating the secondary ID3D12Fence failed")?;
    unsafe { secondary_queue.Wait(&fence, fence_value) }
        .context("secondary queue wait for primary fence failed")?;
    let secondary_base_list: ID3D12CommandList = secondary_command_list.cast()?;
    unsafe { secondary_queue.ExecuteCommandLists(&[Some(secondary_base_list)]) };
    unsafe { secondary_queue.Signal(&secondary_fence, secondary_fence_value) }
        .context("secondary ID3D12CommandQueue::Signal failed")?;
    wait_for_fence(device, &fence, fence_value)?;
    wait_for_fence(device, &secondary_fence, secondary_fence_value)?;
    let first = inspect_readback(
        &first_readback,
        footprint.Footprint.RowPitch,
        total_bytes,
        FIRST_CLEAR,
    )?;
    let second = inspect_readback(
        &second_readback,
        second_footprint.Footprint.RowPitch,
        second_total_bytes,
        SECOND_CLEAR,
    )?;
    let descriptor_overwrites_before = backend.snapshot().descriptor_overwrites;
    unsafe { device.CreateDepthStencilView(&depth, None, dsv) };
    let after_overwrite = backend.snapshot();
    let descriptor_overwrite_invalidated_old_identity =
        after_overwrite.descriptor_overwrites == descriptor_overwrites_before + 1;

    Ok(IterationResult {
        iteration,
        row_pitch_bytes: footprint.Footprint.RowPitch,
        readback_bytes: total_bytes,
        first,
        second,
        queue_count: 2,
        command_list_count: 2,
        fence_values: [fence_value, secondary_fence_value],
        descriptor_overwrite_invalidated_old_identity,
        capture_slots_in_flight_after_iteration: after_overwrite.capture_in_flight,
    })
}

fn depth_resource_desc() -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: u64::from(WIDTH),
        Height: HEIGHT,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_D32_FLOAT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
    }
}

fn heap_properties(heap_type: D3D12_HEAP_TYPE) -> D3D12_HEAP_PROPERTIES {
    D3D12_HEAP_PROPERTIES {
        Type: heap_type,
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    }
}

fn create_readback(
    device: &ID3D12Device,
    source_desc: &D3D12_RESOURCE_DESC,
) -> anyhow::Result<(ID3D12Resource, D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u64)> {
    let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
    let mut rows = 0_u32;
    let mut row_size = 0_u64;
    let mut total_bytes = 0_u64;
    unsafe {
        device.GetCopyableFootprints(
            source_desc,
            0,
            1,
            0,
            Some(&mut footprint),
            Some(&mut rows),
            Some(&mut row_size),
            Some(&mut total_bytes),
        );
    }
    ensure!(
        rows == source_desc.Height,
        "unexpected texture footprint row count: {rows}"
    );
    ensure!(
        row_size == source_desc.Width * 4,
        "unexpected texture footprint row size: {row_size}"
    );
    ensure!(total_bytes > 0, "texture copyable footprint is empty");

    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: total_bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    };
    let readback_heap = heap_properties(D3D12_HEAP_TYPE_READBACK);
    let mut resource = None;
    unsafe {
        device.CreateCommittedResource(
            &readback_heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut resource,
        )
    }
    .context("creating the D3D12 readback buffer failed")?;
    Ok((
        resource.context("CreateCommittedResource returned no readback buffer")?,
        footprint,
        total_bytes,
    ))
}

fn record_transition(
    command_list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    };
    unsafe {
        command_list.ResourceBarrier(std::slice::from_ref(&barrier));
        let transition = &mut *barrier.Anonymous.Transition;
        ManuallyDrop::drop(&mut transition.pResource);
    }
}

fn record_copy(
    command_list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    destination: &ID3D12Resource,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let mut source_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(source.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: 0,
        },
    };
    let mut destination_location = D3D12_TEXTURE_COPY_LOCATION {
        pResource: ManuallyDrop::new(Some(destination.clone())),
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: footprint,
        },
    };
    unsafe {
        command_list.CopyTextureRegion(&destination_location, 0, 0, 0, &source_location, None);
        ManuallyDrop::drop(&mut source_location.pResource);
        ManuallyDrop::drop(&mut destination_location.pResource);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FencePollState {
    Pending,
    Complete,
    DeviceRemoved,
}

const fn classify_fence_poll(completed: u64, value: u64) -> FencePollState {
    if completed == u64::MAX {
        FencePollState::DeviceRemoved
    } else if completed >= value {
        FencePollState::Complete
    } else {
        FencePollState::Pending
    }
}

fn wait_for_fence(device: &ID3D12Device, fence: &ID3D12Fence, value: u64) -> anyhow::Result<()> {
    let started = Instant::now();
    loop {
        match classify_fence_poll(unsafe { fence.GetCompletedValue() }, value) {
            FencePollState::Complete => return Ok(()),
            FencePollState::DeviceRemoved => {
                let reason = unsafe { device.GetDeviceRemovedReason() };
                anyhow::bail!("D3D12 device removed while waiting for fence {value}: {reason:?}");
            }
            FencePollState::Pending => {
                ensure!(
                    started.elapsed() < FENCE_TIMEOUT,
                    "D3D12 fence {value} did not complete within {FENCE_TIMEOUT:?}"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn inspect_readback(
    resource: &ID3D12Resource,
    row_pitch: u32,
    total_bytes: u64,
    expected: f32,
) -> anyhow::Result<DepthReadbackResult> {
    let read_range = D3D12_RANGE {
        Begin: 0,
        End: usize::try_from(total_bytes).context("readback size exceeds usize")?,
    };
    let mut mapped = std::ptr::null_mut();
    unsafe { resource.Map(0, Some(&read_range), Some(&mut mapped)) }
        .context("ID3D12Resource::Map failed")?;
    ensure!(!mapped.is_null(), "ID3D12Resource::Map returned null");

    let bytes = unsafe {
        std::slice::from_raw_parts(
            mapped.cast::<u8>(),
            usize::try_from(total_bytes).context("readback size exceeds usize")?,
        )
    };
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    let mut mismatched_pixels = 0_u64;
    for y in 0..HEIGHT as usize {
        let row_start = y * row_pitch as usize;
        for x in 0..WIDTH as usize {
            let offset = row_start + x * 4;
            let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
            min = min.min(value);
            max = max.max(value);
            if value.to_bits() != expected.to_bits() {
                mismatched_pixels += 1;
            }
        }
    }
    let no_write = D3D12_RANGE { Begin: 0, End: 0 };
    unsafe { resource.Unmap(0, Some(&no_write)) };

    Ok(DepthReadbackResult {
        expected,
        min,
        max,
        mismatched_pixels,
        pixels: u64::from(WIDTH) * u64::from(HEIGHT),
    })
}

#[cfg(test)]
mod tests {
    use super::{FencePollState, classify_fence_poll};

    #[test]
    fn fence_poll_never_treats_device_removal_as_completion() {
        assert_eq!(classify_fence_poll(6, 5), FencePollState::Complete);
        assert_eq!(classify_fence_poll(4, 5), FencePollState::Pending);
        assert_eq!(
            classify_fence_poll(u64::MAX, 5),
            FencePollState::DeviceRemoved
        );
    }
}

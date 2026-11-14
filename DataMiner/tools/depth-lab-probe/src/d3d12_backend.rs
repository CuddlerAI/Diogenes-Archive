//! Fail-closed D3D12 hook, observation, and explicitly armed capture backend.
//!
//! Observation never promotes itself to production readiness. A caller must
//! explicitly arm one already-proven DSV identity before the backend can inject
//! a bounded pass-complete snapshot before the selected depth leaves
//! DEPTH_WRITE on the application's DIRECT list.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::sync::atomic::{
    AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use sha2::{Digest, Sha256};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::{
    D3D_ROOT_SIGNATURE_VERSION_1_1, D3D12_CLEAR_FLAGS, D3D12_CLEAR_VALUE, D3D12_COMMAND_LIST_TYPE,
    D3D12_COMMAND_LIST_TYPE_DIRECT, D3D12_COMMAND_QUEUE_DESC, D3D12_CONSTANT_BUFFER_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE, D3D12_DEPTH_STENCIL_VIEW_DESC, D3D12_DESCRIPTOR_HEAP_DESC,
    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE, D3D12_DESCRIPTOR_HEAP_TYPE,
    D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    D3D12_DESCRIPTOR_RANGE_TYPE_CBV, D3D12_GPU_DESCRIPTOR_HANDLE, D3D12_HEAP_FLAGS,
    D3D12_HEAP_PROPERTIES, D3D12_HEAP_TYPE_UPLOAD, D3D12_RANGE, D3D12_RESOURCE_BARRIER,
    D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES, D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY,
    D3D12_RESOURCE_BARRIER_FLAG_END_ONLY, D3D12_RESOURCE_BARRIER_FLAG_NONE,
    D3D12_RESOURCE_BARRIER_FLAGS, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_DESC1, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_DIMENSION_TEXTURE2D,
    D3D12_RESOURCE_STATE_COMMON, D3D12_RESOURCE_STATE_COPY_SOURCE,
    D3D12_RESOURCE_STATE_DEPTH_WRITE, D3D12_RESOURCE_STATE_RENDER_TARGET, D3D12_RESOURCE_STATES,
    D3D12_RESOURCE_TRANSITION_BARRIER, D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
    D3D12_SHADER_RESOURCE_VIEW_DESC, D3D12_UNORDERED_ACCESS_VIEW_DESC, D3D12CreateDevice,
    D3D12CreateVersionedRootSignatureDeserializer, ID3D12CommandQueue, ID3D12CommandQueue_Vtbl,
    ID3D12DescriptorHeap, ID3D12Device, ID3D12Device_Vtbl, ID3D12GraphicsCommandList,
    ID3D12GraphicsCommandList_Vtbl, ID3D12Heap, ID3D12Resource, ID3D12RootSignature,
    ID3D12VersionedRootSignatureDeserializer,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT,
    DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_R32_TYPELESS,
    DXGI_FORMAT_R32G8X24_TYPELESS,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FULLSCREEN_DESC, IDXGIFactory_Vtbl, IDXGIFactory2, IDXGIFactory2_Vtbl,
    IDXGISwapChain, IDXGISwapChain3,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};
use windows::Win32::System::Performance::QueryPerformanceCounter;
use windows::core::{BOOL, GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

use crate::d3d12_abi::{self, DescriptorHandle, DsvIdentity, QueueId, ResourceId, ResourceState};
use crate::d3d12_capture::{D3d12CaptureRing, D3d12RawDepthFrame};
use crate::d3d12_render_camera::{
    CbvDescriptorStore, D3d12RenderCameraSnapshot, ObservedRootBinding, ProductionDrawDedupKey,
    RenderCameraTracker, RootBindingSource, RootCbvRange, SelectedDrawBindings,
    SelectedDrawObservation,
};
use crate::d3d12_watermark::{D3d12WatermarkRing, WatermarkWriteResult};
use crate::vtable::VtableHook;

mod execute_query_prefetch;
#[cfg(all(feature = "local-perf-lab", debug_assertions))]
pub mod local_perf_lab;
mod mapped_snapshot;
mod original_target_cache;
mod pipeline_depth;
use original_target_cache::OriginalTargetCache;

type CreateCommandQueueFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_COMMAND_QUEUE_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreateCommandListFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    D3D12_COMMAND_LIST_TYPE,
    *mut c_void,
    *mut c_void,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreateDepthStencilViewFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const D3D12_DEPTH_STENCIL_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CreateDescriptorHeapFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_DESCRIPTOR_HEAP_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreateRootSignatureFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const c_void,
    usize,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreateConstantBufferViewFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_CONSTANT_BUFFER_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CreateShaderResourceViewFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CreateUnorderedAccessViewFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    D3D12_CPU_DESCRIPTOR_HANDLE,
);
type CopyDescriptorsFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    *const u32,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    *const u32,
    D3D12_DESCRIPTOR_HEAP_TYPE,
);
type CopyDescriptorsSimpleFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_DESCRIPTOR_HEAP_TYPE,
);
type CreateCommittedResourceFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_FLAGS,
    *const D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_STATES,
    *const D3D12_CLEAR_VALUE,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type CreatePlacedResourceFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    u64,
    *const D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_STATES,
    *const D3D12_CLEAR_VALUE,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
#[allow(dead_code)]
type CreateCommittedResource1Fn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_FLAGS,
    *const D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_STATES,
    *const D3D12_CLEAR_VALUE,
    *mut c_void,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
#[allow(dead_code)]
type CreateCommittedResource2Fn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_FLAGS,
    *const D3D12_RESOURCE_DESC1,
    D3D12_RESOURCE_STATES,
    *const D3D12_CLEAR_VALUE,
    *mut c_void,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
#[allow(dead_code)]
type CreatePlacedResource1Fn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    u64,
    *const D3D12_RESOURCE_DESC1,
    D3D12_RESOURCE_STATES,
    *const D3D12_CLEAR_VALUE,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
#[allow(dead_code)]
type CreateCommittedResource3Fn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_HEAP_PROPERTIES,
    D3D12_HEAP_FLAGS,
    *const D3D12_RESOURCE_DESC1,
    windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT,
    *const D3D12_CLEAR_VALUE,
    *mut c_void,
    u32,
    *const DXGI_FORMAT,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
#[allow(dead_code)]
type CreatePlacedResource2Fn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    u64,
    *const D3D12_RESOURCE_DESC1,
    windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT,
    *const D3D12_CLEAR_VALUE,
    u32,
    *const DXGI_FORMAT,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type ExecuteCommandListsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;
type ResetFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT;
type ResourceBarrierFn = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RESOURCE_BARRIER);
type CopyBufferRegionFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, u64, *mut c_void, u64, u64);
type OmSetRenderTargetsFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
    BOOL,
    *const D3D12_CPU_DESCRIPTOR_HANDLE,
);
type ClearDepthStencilViewFn = unsafe extern "system" fn(
    *mut c_void,
    D3D12_CPU_DESCRIPTOR_HANDLE,
    D3D12_CLEAR_FLAGS,
    f32,
    u8,
    u32,
    *const RECT,
);
type SetGraphicsRootConstantBufferViewFn = unsafe extern "system" fn(*mut c_void, u32, u64);
type SetDescriptorHeapsFn = unsafe extern "system" fn(*mut c_void, u32, *const *mut c_void);
type SetGraphicsRootDescriptorTableFn =
    unsafe extern "system" fn(*mut c_void, u32, D3D12_GPU_DESCRIPTOR_HANDLE);
type SetGraphicsRootSignatureFn = unsafe extern "system" fn(*mut c_void, *mut c_void);
type DrawInstancedFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32);
type DrawIndexedInstancedFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, i32, u32);
type MapResourceFn =
    unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RANGE, *mut *mut c_void) -> HRESULT;
type UnmapResourceFn = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RANGE);
type CreateSwapChainFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const DXGI_SWAP_CHAIN_DESC,
    *mut *mut c_void,
) -> HRESULT;
type RsSetScissorRectsFn = unsafe extern "system" fn(*mut c_void, u32, *const RECT);

type CreateSwapChainForHwndFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    HWND,
    *const DXGI_SWAP_CHAIN_DESC1,
    *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    *mut c_void,
    *mut *mut c_void,
) -> HRESULT;

// These assignments make the aliases fail compilation if the windows-rs ABI
// fields ever change. No hand-maintained community slot/signature is trusted.
#[allow(dead_code)]
fn assert_windows_rs_abi(
    device: &ID3D12Device_Vtbl,
    queue: &ID3D12CommandQueue_Vtbl,
    list: &ID3D12GraphicsCommandList_Vtbl,
) {
    let _: RsSetScissorRectsFn = list.RSSetScissorRects;
    let _: CreateCommandQueueFn = device.CreateCommandQueue;
    let _: CreateCommandListFn = device.CreateCommandList;
    let _: CreateDepthStencilViewFn = device.CreateDepthStencilView;
    let _: CreateDescriptorHeapFn = device.CreateDescriptorHeap;
    let _: CreateRootSignatureFn = device.CreateRootSignature;
    let _: CreateConstantBufferViewFn = device.CreateConstantBufferView;
    let _: CreateShaderResourceViewFn = device.CreateShaderResourceView;
    let _: CreateUnorderedAccessViewFn = device.CreateUnorderedAccessView;
    let _: CopyDescriptorsFn = device.CopyDescriptors;
    let _: CopyDescriptorsSimpleFn = device.CopyDescriptorsSimple;
    let _: CreateCommittedResourceFn = device.CreateCommittedResource;
    let _: CreatePlacedResourceFn = device.CreatePlacedResource;
    let _: ExecuteCommandListsFn = queue.ExecuteCommandLists;
    let _: ReleaseFn = queue.base__.base__.base__.base__.Release;
    let _: ResetFn = list.Reset;
    let _: ResourceBarrierFn = list.ResourceBarrier;
    let _: CopyBufferRegionFn = list.CopyBufferRegion;
    let _: OmSetRenderTargetsFn = list.OMSetRenderTargets;
    let _: ClearDepthStencilViewFn = list.ClearDepthStencilView;
    let _: SetGraphicsRootConstantBufferViewFn = list.SetGraphicsRootConstantBufferView;
    let _: SetDescriptorHeapsFn = list.SetDescriptorHeaps;
    let _: SetGraphicsRootDescriptorTableFn = list.SetGraphicsRootDescriptorTable;
    let _: SetGraphicsRootSignatureFn = list.SetGraphicsRootSignature;
    let _: DrawInstancedFn = list.DrawInstanced;
    let _: DrawIndexedInstancedFn = list.DrawIndexedInstanced;
}

#[allow(dead_code)]
fn assert_windows_rs_dxgi_abi(factory: &IDXGIFactory_Vtbl, factory2: &IDXGIFactory2_Vtbl) {
    let _: CreateSwapChainFn = factory.CreateSwapChain;
    let _: CreateSwapChainForHwndFn = factory2.CreateSwapChainForHwnd;
}

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum HookMethod {
    CreateSwapChain,
    CreateSwapChainForHwnd,
    CreateCommandQueue,
    CreateCommandList,
    CreateDepthStencilView,
    CreateDescriptorHeap,
    CreateRootSignature,
    CreateGraphicsPipelineState,
    CreatePipelineState,
    CreatePipelineLibrary,
    LoadGraphicsPipeline,
    LoadPipeline,
    ReleasePipelineState,
    SetPipelineState,
    CreateConstantBufferView,
    CreateShaderResourceView,
    CreateUnorderedAccessView,
    CopyDescriptors,
    CopyDescriptorsSimple,
    CreateCommittedResource,
    CreatePlacedResource,
    CreateCommittedResource1,
    CreateCommittedResource2,
    CreatePlacedResource1,
    CreateCommittedResource3,
    CreatePlacedResource2,
    ExecuteCommandLists,
    ReleaseQueue,
    ReleaseList,
    ReleaseResource,
    ReleaseDescriptorHeap,
    ReleaseRootSignature,
    Reset,
    RsSetViewports,
    RsSetScissorRects,
    ResourceBarrier,
    CopyBufferRegion,
    OmSetRenderTargets,
    ClearDepthStencilView,
    SetGraphicsRootConstantBufferView,
    SetDescriptorHeaps,
    SetGraphicsRootDescriptorTable,
    SetGraphicsRootSignature,
    DrawInstanced,
    DrawIndexedInstanced,
    MapResource,
    UnmapResource,
}

struct SwapchainRecord {
    queue: ID3D12CommandQueue,
    queue_id: QueueId,
    device: ID3D12Device,
    watermark: Option<D3d12WatermarkRing>,
    present_calls: u64,
    resize_calls: u64,
    watermark_submitted: u64,
    watermark_dropped: u64,
    submitted_sequences: BTreeSet<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct D3d12PresentResult {
    pub present_calls: u64,
    pub watermark_submitted: bool,
    pub watermark_dropped: bool,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HookKey {
    slot: usize,
    method: HookMethod,
}

struct HookRegistry {
    originals: BTreeMap<HookKey, usize>,
    // Guards are intentionally process-lived. Dropping a hook while another
    // application thread is inside its trampoline would be unsafe.
    guards: Vec<VtableHook>,
}

const HOT_LIST_CAPACITY: usize = 2048;
const HOT_LIST_PROBES: usize = 16;
const HOT_ROOT_PARAMETERS: usize = 64;
const ROOT_NONE: u8 = 0;
const ROOT_DIRECT_CBV: u8 = 1;
const ROOT_DESCRIPTOR_TABLE: u8 = 2;
const PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES: usize =
    crate::d3d12_render_camera::MAX_PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES;

/// Fixed-capacity command-list state used by draw/root-binding hooks. Registration and reset are
/// cold operations; the render hot path performs only atomic loads/stores until an exact selected
/// DSV draw needs physical bytes. Slots are process-lived so a concurrent COM release cannot make
/// a hook dereference reclaimed observer memory.
struct HotListSlot {
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    diagnostic_counts: crate::cp2077_camera_diagnostics::DrawCounters,
    // Diagnostic multiplicity only. Per-list try_lock; never changes dedup/admission decisions.
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    diagnostic_weight: Mutex<Option<(u64, u64, Arc<crate::cp2077_camera_diagnostics::DrawWeight>)>>,
    key: AtomicUsize,
    list_tag: AtomicU64,
    generation: AtomicU64,
    draw_ordinal: AtomicU64,
    discovery_camera_samples: AtomicU64,
    selected_pass_draw_ordinal: AtomicU64,
    pass_ordinal: AtomicU64,
    next_pass_ordinal: AtomicU64,
    fallback_draw_proven: AtomicBool,
    bound_selected_token: AtomicU64,
    bound_dsv_flags: AtomicU64,
    descriptor_heaps: [AtomicUsize; 2],
    root_signature: AtomicUsize,
    pipeline_state: AtomicUsize,
    pipeline_hook_available: AtomicBool,
    root_kind: [AtomicU8; HOT_ROOT_PARAMETERS],
    root_value: [AtomicU64; HOT_ROOT_PARAMETERS],
    /// Monotonic recording-local state identity. Exact descriptor generations are cached
    /// separately below; either component changing rejects Draw redundancy without consulting the
    /// process-wide camera tracker.
    production_state_revision: AtomicU64,
    production_cached_revision: AtomicU64,
    production_cached_descriptor_count: AtomicU8,
    production_cached_descriptor_handles: [AtomicU64; PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES],
    production_cached_descriptor_generations: [AtomicU64; PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES],
    production_cached_root_binding_count: AtomicU8,
    production_cached_root_indices: [AtomicU32; PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES],
    production_cached_root_values: [AtomicU64; PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES],
    production_cached_pipeline_state: AtomicUsize,
    /// ExecuteCommandLists needs the global tracker only when an admitted Draw actually staged
    /// work. State changes do not clear this bit: earlier Draws remain pending until Execute/Reset.
    production_camera_work_pending: AtomicBool,
    // Performance-validation counters stay recording-local so Draw callbacks from different
    // command lists do not hammer one process-wide cache line. They are lifetime counters and
    // intentionally survive Reset/Release; checkpoints subtract them at the run boundary.
    production_draw_prefilter_admitted: AtomicU64,
    production_draw_prefilter_rejected: AtomicU64,
    production_draw_lock_immediate: AtomicU64,
    production_draw_lock_contended: AtomicU64,
    production_draw_post_lock_rejected: AtomicU64,
    production_draw_budget_rejected: AtomicU64,
    production_draw_tracker_called: AtomicU64,
    production_draw_redundant: AtomicU64,
}

impl HotListSlot {
    fn new() -> Self {
        Self {
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            diagnostic_weight: Mutex::new(None),
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            diagnostic_counts: crate::cp2077_camera_diagnostics::DrawCounters::new(),
            key: AtomicUsize::new(0),
            list_tag: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            draw_ordinal: AtomicU64::new(0),
            discovery_camera_samples: AtomicU64::new(0),
            selected_pass_draw_ordinal: AtomicU64::new(0),
            pass_ordinal: AtomicU64::new(0),
            next_pass_ordinal: AtomicU64::new(0),
            fallback_draw_proven: AtomicBool::new(false),
            bound_selected_token: AtomicU64::new(0),
            bound_dsv_flags: AtomicU64::new(0),
            descriptor_heaps: std::array::from_fn(|_| AtomicUsize::new(0)),
            root_signature: AtomicUsize::new(0),
            pipeline_state: AtomicUsize::new(0),
            pipeline_hook_available: AtomicBool::new(false),
            root_kind: std::array::from_fn(|_| AtomicU8::new(ROOT_NONE)),
            root_value: std::array::from_fn(|_| AtomicU64::new(0)),
            production_state_revision: AtomicU64::new(1),
            production_cached_revision: AtomicU64::new(0),
            production_cached_descriptor_count: AtomicU8::new(0),
            production_cached_descriptor_handles: std::array::from_fn(|_| AtomicU64::new(0)),
            production_cached_descriptor_generations: std::array::from_fn(|_| AtomicU64::new(0)),
            production_cached_root_binding_count: AtomicU8::new(0),
            production_cached_root_indices: std::array::from_fn(|_| AtomicU32::new(0)),
            production_cached_root_values: std::array::from_fn(|_| AtomicU64::new(0)),
            production_cached_pipeline_state: AtomicUsize::new(0),
            production_camera_work_pending: AtomicBool::new(false),
            production_draw_prefilter_admitted: AtomicU64::new(0),
            production_draw_prefilter_rejected: AtomicU64::new(0),
            production_draw_lock_immediate: AtomicU64::new(0),
            production_draw_lock_contended: AtomicU64::new(0),
            production_draw_post_lock_rejected: AtomicU64::new(0),
            production_draw_budget_rejected: AtomicU64::new(0),
            production_draw_tracker_called: AtomicU64::new(0),
            production_draw_redundant: AtomicU64::new(0),
        }
    }

    fn bump_production_state(&self) {
        self.production_state_revision
            .fetch_add(1, Ordering::AcqRel);
    }

    fn clear_recording_state(&self) {
        self.bump_production_state();
        self.production_cached_descriptor_count
            .store(0, Ordering::Release);
        self.production_cached_root_binding_count
            .store(0, Ordering::Release);
        self.production_camera_work_pending
            .store(false, Ordering::Release);
        self.draw_ordinal.store(0, Ordering::Release);
        self.discovery_camera_samples.store(0, Ordering::Release);
        self.selected_pass_draw_ordinal.store(0, Ordering::Release);
        self.pass_ordinal.store(0, Ordering::Release);
        self.next_pass_ordinal.store(0, Ordering::Release);
        self.fallback_draw_proven.store(false, Ordering::Release);
        self.bound_selected_token.store(0, Ordering::Release);
        self.bound_dsv_flags.store(0, Ordering::Release);
        for heap in &self.descriptor_heaps {
            heap.store(0, Ordering::Release);
        }
        self.root_signature.store(0, Ordering::Release);
        self.pipeline_state.store(0, Ordering::Release);
        for index in 0..HOT_ROOT_PARAMETERS {
            self.root_value[index].store(0, Ordering::Relaxed);
            self.root_kind[index].store(ROOT_NONE, Ordering::Release);
        }
    }
}

struct HotListRegistry {
    slots: Vec<HotListSlot>,
    drops: AtomicU64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProductionDrawFilter {
    sources: Box<[(String, u32)]>,
}

impl ProductionDrawFilter {
    fn from_binding(binding: &depth_lab::D3d12CameraBinding) -> Self {
        let sources = binding
            .view_sources
            .iter()
            .map(|view| {
                (
                    view.source.descriptor_table.root_signature_sha256.clone(),
                    view.source.descriptor_table.root_parameter_index,
                )
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { sources }
    }
}

impl HotListRegistry {
    fn new() -> Self {
        Self {
            slots: (0..HOT_LIST_CAPACITY).map(|_| HotListSlot::new()).collect(),
            drops: AtomicU64::new(0),
        }
    }

    fn start(raw: usize) -> usize {
        raw.wrapping_mul(0x9e37_79b1) % HOT_LIST_CAPACITY
    }

    fn find(&self, raw: usize) -> Option<&HotListSlot> {
        if raw == 0 {
            return None;
        }
        let start = Self::start(raw);
        (0..HOT_LIST_PROBES)
            .map(|step| &self.slots[(start + step) % HOT_LIST_CAPACITY])
            .find(|slot| slot.key.load(Ordering::Acquire) == raw)
    }

    fn register(&self, raw: usize, list_tag: u64) {
        if raw == 0 {
            return;
        }
        if let Some(slot) = self.find(raw) {
            slot.list_tag.store(list_tag, Ordering::Release);
            return;
        }
        let start = Self::start(raw);
        for step in 0..HOT_LIST_PROBES {
            let slot = &self.slots[(start + step) % HOT_LIST_CAPACITY];
            if slot
                .key
                .compare_exchange(0, raw, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                slot.list_tag.store(list_tag, Ordering::Release);
                slot.generation.store(1, Ordering::Release);
                slot.clear_recording_state();
                slot.pipeline_hook_available.store(false, Ordering::Release);
                return;
            }
        }
        self.drops.fetch_add(1, Ordering::Relaxed);
    }

    fn reset(&self, raw: usize) {
        if let Some(slot) = self.find(raw) {
            slot.generation.fetch_add(1, Ordering::AcqRel);
            slot.clear_recording_state();
        }
    }

    fn reset_with_pipeline(&self, raw: usize, initial_state: usize) {
        self.reset(raw);
        self.bind_pipeline_state(raw, initial_state);
    }

    fn set_pipeline_hook_available(&self, raw: usize, available: bool) {
        if let Some(slot) = self.find(raw)
            && slot
                .pipeline_hook_available
                .swap(available, Ordering::AcqRel)
                != available
        {
            slot.bump_production_state();
        }
    }

    fn bind_pipeline_state(&self, raw: usize, state: usize) {
        if let Some(slot) = self.find(raw) {
            slot.pipeline_state.store(state, Ordering::Release);
        }
    }

    fn release(&self, raw: usize) {
        if let Some(slot) = self.find(raw) {
            slot.clear_recording_state();
            slot.pipeline_hook_available.store(false, Ordering::Release);
            slot.list_tag.store(0, Ordering::Release);
            slot.generation.store(0, Ordering::Release);
            slot.key.store(0, Ordering::Release);
        }
    }

    fn bind_selected_depth(&self, raw: usize, token: u64) {
        if let Some(slot) = self.find(raw) {
            slot.bump_production_state();
            let pass_ordinal = slot.next_pass_ordinal.fetch_add(1, Ordering::AcqRel);
            slot.pass_ordinal.store(pass_ordinal, Ordering::Release);
            slot.selected_pass_draw_ordinal.store(0, Ordering::Release);
            slot.fallback_draw_proven.store(false, Ordering::Release);
            slot.bound_selected_token.store(token, Ordering::Release);
            slot.bound_dsv_flags.store(0, Ordering::Release);
        }
    }

    fn unbind_depth(&self, raw: usize) {
        if let Some(slot) = self.find(raw) {
            slot.bump_production_state();
            slot.bound_dsv_flags.store(0, Ordering::Release);
            // KCD2's sealed pass identity counts non-null DSV binds only.
            // A color-only OM call ends selection but is not a new depth pass.
            slot.bound_selected_token.store(0, Ordering::Release);
            slot.fallback_draw_proven.store(false, Ordering::Release);
        }
    }

    fn bind_root(&self, raw: usize, root: u32, kind: u8, value: u64) {
        let Ok(index) = usize::try_from(root) else {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if index >= HOT_ROOT_PARAMETERS {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(slot) = self.find(raw) else {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let next_kind = if value == 0 { ROOT_NONE } else { kind };
        if slot.root_kind[index].load(Ordering::Acquire) == next_kind
            && slot.root_value[index].load(Ordering::Acquire) == value
        {
            return;
        }
        slot.root_value[index].store(value, Ordering::Relaxed);
        slot.root_kind[index].store(next_kind, Ordering::Release);
        // Do not invalidate the camera representative for unrelated material roots. The exact
        // authored camera root index/value pairs are published with the descriptor generations
        // and checked independently before a Draw is represented.
    }

    fn bind_heaps(&self, raw: usize, heaps: [usize; 2]) {
        let Some(slot) = self.find(raw) else {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if slot
            .descriptor_heaps
            .iter()
            .zip(heaps)
            .all(|(current, next)| current.load(Ordering::Acquire) == next)
        {
            return;
        }
        for (target, value) in slot.descriptor_heaps.iter().zip(heaps) {
            target.store(value, Ordering::Release);
        }
        // D3D12 invalidates descriptor-table root arguments when descriptor heaps change.
        for index in 0..HOT_ROOT_PARAMETERS {
            if slot.root_kind[index].load(Ordering::Acquire) == ROOT_DESCRIPTOR_TABLE {
                slot.root_value[index].store(0, Ordering::Relaxed);
                slot.root_kind[index].store(ROOT_NONE, Ordering::Release);
            }
        }
        slot.bump_production_state();
    }

    fn bind_root_signature(&self, raw: usize, root_signature: usize) {
        let Some(slot) = self.find(raw) else {
            self.drops.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if slot.root_signature.swap(root_signature, Ordering::AcqRel) == root_signature {
            return;
        }
        for index in 0..HOT_ROOT_PARAMETERS {
            slot.root_value[index].store(0, Ordering::Relaxed);
            slot.root_kind[index].store(ROOT_NONE, Ordering::Release);
        }
        slot.bump_production_state();
    }

    /// Copy-time stamp from the same recording-local counter used by
    /// `SelectedDrawBindings`, including its direct root-CBV bindings. This is
    /// not the legacy `RenderCameraTracker::observe_draw` counter. Missing or
    /// saturated ordinals cannot establish ordering and stay unknown.
    fn recorded_draw_count(&self, raw: usize) -> Option<u32> {
        let count = self.find(raw)?.draw_ordinal.load(Ordering::Acquire);
        u32::try_from(count).ok().filter(|count| *count < u32::MAX)
    }

    fn selected_draw_ordinals(&self, raw: usize, selected_token: u64) -> Option<(u32, u32, u32)> {
        let slot = self.find(raw)?;
        let global_ordinal = slot
            .draw_ordinal
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        if selected_token == 0
            || slot.bound_selected_token.load(Ordering::Acquire) != selected_token
        {
            return None;
        }
        let pass_ordinal = slot
            .selected_pass_draw_ordinal
            .fetch_add(1, Ordering::Relaxed);
        Some((
            global_ordinal.try_into().unwrap_or(u32::MAX),
            slot.pass_ordinal
                .load(Ordering::Acquire)
                .try_into()
                .unwrap_or(u32::MAX),
            pass_ordinal.try_into().unwrap_or(u32::MAX),
        ))
    }

    fn root_signature(&self, raw: usize) -> Option<usize> {
        self.find(raw)
            .map(|slot| slot.root_signature.load(Ordering::Acquire))
    }

    fn production_draw_filter_match(
        &self,
        raw: usize,
        root_signature_sha256: &str,
        filter: &ProductionDrawFilter,
    ) -> Option<(&HotListSlot, bool)> {
        let slot = self.find(raw)?;
        let matched = filter.sources.iter().any(|(expected_sha256, root)| {
            expected_sha256 == root_signature_sha256
                && usize::try_from(*root).ok().is_some_and(|index| {
                    index < HOT_ROOT_PARAMETERS
                        && slot.root_kind[index].load(Ordering::Acquire) == ROOT_DESCRIPTOR_TABLE
                })
        });
        Some((slot, matched))
    }

    fn production_draw_metrics(&self) -> [u64; 8] {
        self.slots.iter().fold([0; 8], |mut totals, slot| {
            for (total, value) in totals.iter_mut().zip([
                &slot.production_draw_prefilter_admitted,
                &slot.production_draw_prefilter_rejected,
                &slot.production_draw_lock_immediate,
                &slot.production_draw_lock_contended,
                &slot.production_draw_post_lock_rejected,
                &slot.production_draw_budget_rejected,
                &slot.production_draw_tracker_called,
                &slot.production_draw_redundant,
            ]) {
                *total = total.saturating_add(value.load(Ordering::Relaxed));
            }
            totals
        })
    }

    /// List recording owns this quota. Neither Present nor DSV rebind replenishes it.
    /// D3D12 command-list recording itself is single-threaded; the atomics share the existing
    /// lifetime-safe hot slots with Reset/Release. No matrix bytes or bindings are allocated here.
    fn sample_discovery_camera_draw(&self, raw: usize) -> bool {
        self.find(raw).is_some_and(|slot| {
            slot.discovery_camera_samples
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    (used < u64::from(
                        crate::d3d12_render_camera::MAX_SELECTED_DRAWS_SCANNED_PER_LIST_GENERATION,
                    ))
                    .then_some(used.saturating_add(1))
                })
                .is_ok()
        })
    }

    fn mark_fallback_draw_proven(&self, raw: usize) {
        if let Some(slot) = self.find(raw) {
            slot.fallback_draw_proven.store(true, Ordering::Release);
        }
    }

    fn fallback_draw_proven(&self, raw: usize) -> bool {
        self.find(raw)
            .is_some_and(|slot| slot.fallback_draw_proven.load(Ordering::Acquire))
    }

    fn set_bound_dsv_flags(&self, raw: usize, flags: Option<i32>) {
        if let Some(slot) = self.find(raw) {
            // Bit 32 marks known; an observed zero is not missing evidence.
            let packed = flags.map_or(0, |v| (1u64 << 32) | u64::from(v as u32));
            if slot.bound_dsv_flags.swap(packed, Ordering::AcqRel) != packed {
                slot.bump_production_state();
            }
        }
    }

    fn touch_production_state(&self, raw: usize) {
        if let Some(slot) = self.find(raw) {
            slot.bump_production_state();
        }
    }

    fn mark_production_depth_copy_recorded(&self, raw: usize) {
        // A pre-copy representative cannot cover a later Draw: the order gate must observe both
        // sides of the injected copy boundary even when every ordinary D3D12 binding is equal.
        self.touch_production_state(raw);
    }

    fn production_draw_is_redundant(
        &self,
        slot: &HotListSlot,
        descriptors: &CbvDescriptorStore,
        psos: &pipeline_depth::PsoRegistry,
    ) -> bool {
        let state_revision = slot.production_state_revision.load(Ordering::Acquire);
        if slot.production_cached_revision.load(Ordering::Acquire) != state_revision {
            return false;
        }
        let count = usize::from(
            slot.production_cached_descriptor_count
                .load(Ordering::Acquire),
        );
        if count == 0 || count > PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES {
            return false;
        }
        let root_count = usize::from(
            slot.production_cached_root_binding_count
                .load(Ordering::Acquire),
        );
        if root_count == 0 || root_count > PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES {
            return false;
        }
        let current_pipeline = if slot.pipeline_hook_available.load(Ordering::Acquire) {
            slot.pipeline_state.load(Ordering::Acquire)
        } else {
            // Without complete SetPipelineState coverage we cannot prove that the current Draw
            // still has the representative's depth/stencil semantics. Unknown must fail closed.
            return false;
        };
        if !psos.production_semantically_equal(
            slot.production_cached_pipeline_state
                .load(Ordering::Acquire),
            current_pipeline,
        ) {
            return false;
        }
        for index in 0..root_count {
            let Ok(root) =
                usize::try_from(slot.production_cached_root_indices[index].load(Ordering::Acquire))
            else {
                return false;
            };
            if root >= HOT_ROOT_PARAMETERS
                || slot.root_kind[root].load(Ordering::Acquire) != ROOT_DESCRIPTOR_TABLE
                || slot.root_value[root].load(Ordering::Acquire)
                    != slot.production_cached_root_values[index].load(Ordering::Acquire)
            {
                return false;
            }
        }
        for index in 0..count {
            let handle = slot.production_cached_descriptor_handles[index].load(Ordering::Acquire);
            let generation =
                slot.production_cached_descriptor_generations[index].load(Ordering::Acquire);
            if handle == 0 || descriptors.generation(handle) != Some(generation) {
                return false;
            }
        }
        // A Reset/state hook racing this read is invalid D3D12 list use, but the second read keeps
        // the optimization fail-closed even in that case.
        slot.production_state_revision.load(Ordering::Acquire) == state_revision
            && slot.production_cached_revision.load(Ordering::Acquire) == state_revision
    }

    fn publish_production_draw_observation(
        &self,
        raw: usize,
        expected_state_revision: u64,
        observation: SelectedDrawObservation,
    ) {
        let Some(slot) = self.find(raw) else {
            return;
        };
        if observation.staged {
            slot.production_camera_work_pending
                .store(true, Ordering::Release);
        }
        let Some(ProductionDrawDedupKey {
            descriptor_identities,
            descriptor_count,
            root_bindings,
            root_binding_count,
        }) = observation.production_dedup_key
        else {
            return;
        };
        let count = usize::from(descriptor_count);
        let root_count = usize::from(root_binding_count);
        if !observation.staged
            || count == 0
            || count > PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES
            || root_count == 0
            || root_count > PRODUCTION_DRAW_DESCRIPTOR_IDENTITIES
        {
            return;
        }
        if slot.production_state_revision.load(Ordering::Acquire) != expected_state_revision {
            return;
        }
        for (index, (handle, generation)) in
            descriptor_identities.into_iter().take(count).enumerate()
        {
            slot.production_cached_descriptor_handles[index].store(handle, Ordering::Relaxed);
            slot.production_cached_descriptor_generations[index]
                .store(generation, Ordering::Relaxed);
        }
        for (index, (root, value)) in root_bindings.into_iter().take(root_count).enumerate() {
            slot.production_cached_root_indices[index].store(root, Ordering::Relaxed);
            slot.production_cached_root_values[index].store(value, Ordering::Relaxed);
        }
        slot.production_cached_descriptor_count
            .store(descriptor_count, Ordering::Release);
        slot.production_cached_root_binding_count
            .store(root_binding_count, Ordering::Release);
        slot.production_cached_pipeline_state.store(
            if slot.pipeline_hook_available.load(Ordering::Acquire) {
                slot.pipeline_state.load(Ordering::Acquire)
            } else {
                0
            },
            Ordering::Release,
        );
        slot.production_cached_revision
            .store(expected_state_revision, Ordering::Release);
    }

    fn has_production_camera_work(&self, lists: &[usize]) -> bool {
        lists.iter().any(|raw| {
            self.find(*raw)
                .is_some_and(|slot| slot.production_camera_work_pending.load(Ordering::Acquire))
        })
    }

    fn clear_production_camera_work(&self, lists: &[usize]) {
        for raw in lists {
            if let Some(slot) = self.find(*raw) {
                slot.production_camera_work_pending
                    .store(false, Ordering::Release);
            }
        }
    }

    /// Close the one-time transition from discovery/legacy observation into bound production.
    /// A Draw that read the old mode before publication can still stage tracker work after the
    /// production mutex is released. Conservatively flag every already-registered list so its
    /// first subsequent Execute cannot skip that work.
    fn mark_registered_production_camera_work(&self) {
        for slot in &self.slots {
            if slot.key.load(Ordering::Acquire) != 0 {
                slot.production_camera_work_pending
                    .store(true, Ordering::Release);
            }
        }
    }

    fn bound_dsv_flags(&self, raw: usize) -> Option<i32> {
        let packed = self.find(raw)?.bound_dsv_flags.load(Ordering::Acquire);
        (packed & (1u64 << 32) != 0).then_some(packed as u32 as i32)
    }

    fn selected_draw_bindings(
        &self,
        raw: usize,
        draw_ordinal: u32,
        pass_ordinal: u32,
        draw_ordinal_in_pass: u32,
    ) -> Option<SelectedDrawBindings> {
        let slot = self.find(raw)?;
        let mut roots = Vec::with_capacity(HOT_ROOT_PARAMETERS);
        for index in 0..HOT_ROOT_PARAMETERS {
            let kind = slot.root_kind[index].load(Ordering::Acquire);
            let source = match kind {
                ROOT_DIRECT_CBV => RootBindingSource::Direct,
                ROOT_DESCRIPTOR_TABLE => RootBindingSource::DescriptorTableBase,
                _ => continue,
            };
            roots.push(ObservedRootBinding {
                root_parameter_index: index as u32,
                source,
                value: slot.root_value[index].load(Ordering::Relaxed),
            });
        }
        Some(SelectedDrawBindings {
            // Without SetPipelineState coverage, an initial PSO could be stale.
            pipeline_state: if slot.pipeline_hook_available.load(Ordering::Acquire) {
                slot.pipeline_state.load(Ordering::Acquire)
            } else {
                0
            },
            pass_context: None,
            pipeline_depth: None,
            list_generation: slot.generation.load(Ordering::Acquire),
            draw_ordinal,
            pass_ordinal,
            draw_ordinal_in_pass,
            descriptor_heaps: [
                slot.descriptor_heaps[0].load(Ordering::Acquire),
                slot.descriptor_heaps[1].load(Ordering::Acquire),
            ],
            root_signature: slot.root_signature.load(Ordering::Acquire),
            roots,
        })
    }

    #[cfg(test)]
    fn selected_draw(&self, raw: usize, selected_token: u64) -> Option<SelectedDrawBindings> {
        let (ordinal, pass, in_pass) = self.selected_draw_ordinals(raw, selected_token)?;
        self.selected_draw_bindings(raw, ordinal, pass, in_pass)
    }
}

impl HookRegistry {
    fn new() -> Self {
        Self {
            originals: BTreeMap::new(),
            guards: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct QueueRecord {
    id: QueueId,
    kind: D3D12_COMMAND_LIST_TYPE,
}

#[derive(Clone, Copy, Debug)]
struct ResourceRecord {
    id: ResourceId,
    epoch: u64,
    state: Option<ResourceState>,
    // Discovery keeps the exact state bits even when the production capture
    // state machine deliberately rejects that combined/read-only state.
    raw_state: Option<i32>,
    single_mip_depth_plane: bool,
    separate_stencil_plane: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DepthTransitionScope {
    Depth,
    Stencil,
    Unsupported,
}

impl ResourceRecord {
    fn transition_scope(&self, subresource: u32) -> DepthTransitionScope {
        if subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES
            || (self.single_mip_depth_plane && subresource == 0)
        {
            DepthTransitionScope::Depth
        } else if self.single_mip_depth_plane && self.separate_stencil_plane && subresource == 1 {
            DepthTransitionScope::Stencil
        } else {
            DepthTransitionScope::Unsupported
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct DescriptorRecord {
    dsv_flags: Option<i32>,
    identity: Option<DsvIdentity>,
    generation: u64,
}

#[derive(Clone, Copy, Debug)]
struct DsvMetadata {
    resource_dimension: i32,
    width: u64,
    height: u32,
    depth_or_array_size: u16,
    mip_levels: u16,
    sample_count: u32,
    sample_quality: u32,
    resource_format: i32,
    resource_layout: i32,
    resource_flags: i32,
    dsv_format: i32,
    dsv_dimension: i32,
    dsv_flags: i32,
    view_desc_explicit: bool,
}

#[derive(Clone, Debug)]
struct CandidateRecord {
    identity: DsvIdentity,
    metadata: DsvMetadata,
    clear_calls: u64,
    eligible_clear_calls: u64,
    same_list_capture_eligible_clear_calls: u64,
    execute_calls: u64,
    present_intervals_with_clear: u64,
    present_intervals_with_one_clear: u64,
    present_intervals_with_multiple_clears: u64,
    max_clears_per_present_interval: u64,
    first_present_call_with_clear: Option<u64>,
    last_present_call_with_clear: Option<u64>,
    present_intervals_with_same_list_capture_eligible_clear: u64,
    bind_calls: u64,
    clear_while_bound_calls: u64,
    clear_parameters: BTreeMap<(i32, u32, u8, u32), u64>,
    raw_states_before_clear: BTreeMap<Option<i32>, u64>,
    transition_pairs: BTreeMap<(i32, i32, u32, bool), u64>,
    distinct_lists: BTreeSet<usize>,
    distinct_queues: BTreeSet<u64>,
    pending_clears: u64,
    pending_same_list_capture_eligible_clears: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct D3d12ClearParameterObservation {
    pub clear_flags: i32,
    pub depth_value_bits: u32,
    pub stencil: u8,
    pub rect_count: u32,
    pub calls: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct D3d12RawStateObservation {
    pub state: Option<i32>,
    pub calls: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct D3d12TransitionPairObservation {
    pub state_before: i32,
    pub state_after: i32,
    pub subresource: u32,
    pub split: bool,
    pub calls: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct D3d12BlockerCountObservation {
    pub class: String,
    pub count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct D3d12DepthCandidateObservation {
    pub candidate_id: String,
    pub descriptor_generation: u64,
    pub resource_epoch: u64,
    pub width: u64,
    pub height: u32,
    pub sample_count: u32,
    pub sample_quality: u32,
    pub resource_format: i32,
    pub dsv_format: i32,
    pub clear_calls: u64,
    pub eligible_clear_calls: u64,
    pub same_list_capture_eligible_clear_calls: u64,
    pub execute_calls: u64,
    pub present_intervals_with_clear: u64,
    pub present_intervals_with_one_clear: u64,
    pub present_intervals_with_multiple_clears: u64,
    pub max_clears_per_present_interval: u64,
    pub first_present_call_with_clear: Option<u64>,
    pub last_present_call_with_clear: Option<u64>,
    /// Executed clears since the last accounted Present; resource liveness alone does not
    /// establish that this DSV participates in the interval currently being observed.
    pub pending_clear_calls: u64,
    pub present_intervals_with_same_list_capture_eligible_clear: u64,
    pub bind_calls: u64,
    pub clear_while_bound_calls: u64,
    pub clear_parameters: Vec<D3d12ClearParameterObservation>,
    pub raw_states_before_clear: Vec<D3d12RawStateObservation>,
    pub transition_pairs: Vec<D3d12TransitionPairObservation>,
    pub distinct_command_lists: usize,
    pub distinct_direct_queues: usize,
    pub current: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct D3d12DepthSelector {
    pub candidate_id: String,
    pub width: u32,
    pub height: u32,
    pub sample_count: u32,
    pub accepted_formats: Vec<i32>,
    /// Cross-launch stable selector for a production profile. Discovery/raw-demo
    /// callers leave this absent and keep the legacy process-local candidate id.
    pub stable_selection: Option<depth_lab::D3d12DepthSelection>,
}

fn depth_format_value(format: depth_lab::DiscoveryDepthFormat) -> i32 {
    match format {
        depth_lab::DiscoveryDepthFormat::R24G8Typeless => DXGI_FORMAT_R24G8_TYPELESS.0,
        depth_lab::DiscoveryDepthFormat::D24UnormS8Uint => DXGI_FORMAT_D24_UNORM_S8_UINT.0,
        depth_lab::DiscoveryDepthFormat::R32Typeless => DXGI_FORMAT_R32_TYPELESS.0,
        depth_lab::DiscoveryDepthFormat::D32Float => DXGI_FORMAT_D32_FLOAT.0,
        depth_lab::DiscoveryDepthFormat::R32G8X24Typeless => DXGI_FORMAT_R32G8X24_TYPELESS.0,
        depth_lab::DiscoveryDepthFormat::D32FloatS8X24Uint => DXGI_FORMAT_D32_FLOAT_S8X24_UINT.0,
    }
}

fn stable_candidate_matches(
    candidate: &CandidateRecord,
    selection: &depth_lab::D3d12DepthSelection,
) -> bool {
    let resource = selection.resource;
    let view = selection.view;
    candidate.metadata.resource_dimension == resource.dimension
        && candidate.metadata.width == resource.width
        && candidate.metadata.height == resource.height
        && candidate.metadata.depth_or_array_size == resource.depth_or_array_size
        && candidate.metadata.mip_levels == resource.mip_levels
        && candidate.metadata.sample_count == resource.sample_count
        && candidate.metadata.sample_quality == resource.sample_quality
        && candidate.metadata.resource_format == depth_format_value(resource.format)
        && candidate.metadata.resource_layout == resource.layout
        && candidate.metadata.resource_flags == resource.flags
        && candidate.metadata.view_desc_explicit
        && candidate.metadata.dsv_format == depth_format_value(view.format)
        && candidate.metadata.dsv_dimension == view.dimension
        && candidate.metadata.dsv_flags == view.flags
}

#[derive(Clone, Copy, Debug)]
struct RawTransition {
    resource: usize,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
    subresource: u32,
    split: bool,
}

#[derive(Clone, Copy, Debug)]
enum ListEvent {
    Transition(RawTransition),
    BindDepth(Option<DsvIdentity>),
    ClearDepth {
        identity: Option<DsvIdentity>,
        flags: i32,
        depth_value_bits: u32,
        stencil: u8,
        rect_count: u32,
        same_list_capture_eligible: bool,
    },
}

#[derive(Debug)]
struct ListRecord {
    id: u64,
    kind: D3D12_COMMAND_LIST_TYPE,
    generation: u64,
    events: Vec<ListEvent>,
    submitted_queue: Option<QueueId>,
    bound_depth: Option<DsvIdentity>,
    // Exact raw D3D12 state bitsets observed while this command list is being
    // recorded. Read states are legally composable (CP2077 uses 0xE0 and 0x40),
    // so reducing them to the small production-copy enum loses the subsequent
    // explicit transition back to DEPTH_WRITE.
    recording_states: BTreeMap<ResourceId, i32>,
}

#[derive(Debug)]
struct ObservationState {
    resource_epoch: u64,
    next_resource_id: u64,
    next_queue_id: u64,
    next_list_id: u64,
    queues: BTreeMap<usize, QueueRecord>,
    lists: BTreeMap<usize, ListRecord>,
    resources: BTreeMap<usize, ResourceRecord>,
    descriptors: BTreeMap<u64, DescriptorRecord>,
    candidates: BTreeMap<(u64, u64, u64), CandidateRecord>,
    present_calls: u64,
    candidates_dropped: u64,
    create_command_queue_calls: u64,
    create_command_list_calls: u64,
    create_dsv_calls: u64,
    resource_barrier_calls: u64,
    om_set_render_targets_calls: u64,
    clear_depth_calls: u64,
    execute_command_lists_calls: u64,
    executed_lists: u64,
    eligible_pre_clear_snapshots: u64,
    descriptor_overwrites: u64,
    blockers: Vec<String>,
    blocker_counts: BTreeMap<String, u64>,
    structural_failure: bool,
}

const MAX_PERSISTED_BLOCKER_DETAILS: usize = 128;

fn blocker_class(message: &str) -> &'static str {
    if message.contains("not a DIRECT queue") {
        "queue_not_direct"
    } else if message.contains("unsupported DSV StateBefore") {
        "unsupported_dsv_state_before"
    } else if message.contains("unsupported DSV StateAfter") {
        "unsupported_dsv_state_after"
    } else if message.contains("per-subresource DSV state tracking") {
        "per_subresource_dsv_state"
    } else if message.contains("unsupported split barrier") {
        "split_dsv_barrier"
    } else if message.contains("StateBefore mismatch") {
        "dsv_state_before_mismatch"
    } else if message.contains("no proven DEPTH_WRITE") {
        "clear_without_proven_depth_write"
    } else if message.contains("would block") || message.contains("association") {
        "capture_association_busy"
    } else if message.contains("stale DSV") {
        "stale_dsv_identity"
    } else if message.contains("unknown") || message.contains("null") {
        "unknown_or_null_object"
    } else if message.contains("registered twice") || message.contains("overwritten") {
        "object_identity_reuse"
    } else {
        "other"
    }
}

impl ObservationState {
    fn new() -> Self {
        Self {
            resource_epoch: 1,
            next_resource_id: 1,
            next_queue_id: 1,
            next_list_id: 1,
            queues: BTreeMap::new(),
            lists: BTreeMap::new(),
            resources: BTreeMap::new(),
            descriptors: BTreeMap::new(),
            candidates: BTreeMap::new(),
            present_calls: 0,
            candidates_dropped: 0,
            create_command_queue_calls: 0,
            create_command_list_calls: 0,
            create_dsv_calls: 0,
            resource_barrier_calls: 0,
            om_set_render_targets_calls: 0,
            clear_depth_calls: 0,
            execute_command_lists_calls: 0,
            executed_lists: 0,
            eligible_pre_clear_snapshots: 0,
            descriptor_overwrites: 0,
            blockers: Vec::new(),
            blocker_counts: BTreeMap::new(),
            structural_failure: false,
        }
    }

    fn block(&mut self, message: impl Into<String>) {
        self.structural_failure = true;
        self.reject_candidate(message);
    }

    // An ineligible discovery event is not proof that the selected copy's hooks are corrupt.
    // Keep every diagnostic while actual capture still requires exact per-copy state/identity.
    fn reject_candidate(&mut self, message: impl Into<String>) {
        let message = message.into();
        *self
            .blocker_counts
            .entry(blocker_class(&message).to_string())
            .or_default() += 1;
        if self.blockers.len() < MAX_PERSISTED_BLOCKER_DETAILS
            && !self.blockers.iter().any(|existing| existing == &message)
        {
            self.blockers.push(message);
        }
    }

    fn register_queue(&mut self, object: usize, kind: D3D12_COMMAND_LIST_TYPE) {
        self.create_command_queue_calls += 1;
        if object == 0 {
            self.block("CreateCommandQueue returned a null object");
            return;
        }
        if self.queues.contains_key(&object) {
            self.block("an already-known queue object was registered twice");
            return;
        }
        let id = QueueId(self.next_queue_id);
        self.next_queue_id = self.next_queue_id.saturating_add(1);
        self.queues.insert(object, QueueRecord { id, kind });
    }

    fn register_list(&mut self, object: usize, kind: D3D12_COMMAND_LIST_TYPE) -> Option<u64> {
        self.create_command_list_calls += 1;
        if object == 0 {
            self.block("CreateCommandList returned a null object");
            return None;
        }
        if self.lists.contains_key(&object) {
            self.block("an already-known command-list object was registered twice");
            return None;
        }
        let list_tag = self.next_list_id;
        self.lists.insert(
            object,
            ListRecord {
                id: list_tag,
                kind,
                generation: 1,
                events: Vec::new(),
                submitted_queue: None,
                bound_depth: None,
                recording_states: BTreeMap::new(),
            },
        );
        self.next_list_id = self.next_list_id.saturating_add(1);
        Some(list_tag)
    }

    fn write_dsv(&mut self, handle: u64, resource: usize, metadata: Option<DsvMetadata>) {
        self.create_dsv_calls += 1;
        if handle == 0 {
            self.block("CreateDepthStencilView used a zero descriptor handle");
            return;
        }
        let next_generation = self
            .descriptors
            .get(&handle)
            .map_or(1, |record| record.generation.saturating_add(1));
        if next_generation > 1 {
            self.descriptor_overwrites += 1;
        }
        let identity = if resource == 0 {
            None
        } else {
            let resource_record = self.resources.entry(resource).or_insert_with(|| {
                let id = ResourceId(self.next_resource_id);
                self.next_resource_id = self.next_resource_id.saturating_add(1);
                ResourceRecord {
                    id,
                    epoch: self.resource_epoch,
                    state: None,
                    raw_state: None,
                    single_mip_depth_plane: metadata.is_some_and(|value| {
                        value.resource_dimension == D3D12_RESOURCE_DIMENSION_TEXTURE2D.0
                            && value.mip_levels == 1
                            && value.depth_or_array_size == 1
                    }),
                    separate_stencil_plane: metadata.is_some_and(|value| {
                        [
                            DXGI_FORMAT_R32G8X24_TYPELESS.0,
                            DXGI_FORMAT_D32_FLOAT_S8X24_UINT.0,
                            DXGI_FORMAT_R24G8_TYPELESS.0,
                            DXGI_FORMAT_D24_UNORM_S8_UINT.0,
                        ]
                        .contains(&value.resource_format)
                    }),
                }
            });
            Some(DsvIdentity {
                handle: DescriptorHandle(handle),
                descriptor_generation: next_generation,
                resource: resource_record.id,
                resource_epoch: resource_record.epoch,
            })
        };
        self.descriptors.insert(
            handle,
            DescriptorRecord {
                dsv_flags: metadata.map(|value| value.dsv_flags),
                identity,
                generation: next_generation,
            },
        );
        if let (Some(identity), Some(metadata)) = (identity, metadata) {
            let key = (
                identity.resource.0,
                identity.descriptor_generation,
                identity.resource_epoch,
            );
            if self.candidates.len() >= 64 && !self.candidates.contains_key(&key) {
                self.retire_unused_candidates();
            }
            if self.candidates.len() < 64 || self.candidates.contains_key(&key) {
                self.candidates.entry(key).or_insert(CandidateRecord {
                    identity,
                    metadata,
                    clear_calls: 0,
                    eligible_clear_calls: 0,
                    same_list_capture_eligible_clear_calls: 0,
                    execute_calls: 0,
                    present_intervals_with_clear: 0,
                    present_intervals_with_one_clear: 0,
                    present_intervals_with_multiple_clears: 0,
                    max_clears_per_present_interval: 0,
                    first_present_call_with_clear: None,
                    last_present_call_with_clear: None,
                    present_intervals_with_same_list_capture_eligible_clear: 0,
                    bind_calls: 0,
                    clear_while_bound_calls: 0,
                    clear_parameters: BTreeMap::new(),
                    raw_states_before_clear: BTreeMap::new(),
                    transition_pairs: BTreeMap::new(),
                    distinct_lists: BTreeSet::new(),
                    distinct_queues: BTreeSet::new(),
                    pending_clears: 0,
                    pending_same_list_capture_eligible_clears: 0,
                });
            } else {
                self.candidates_dropped = self.candidates_dropped.saturating_add(1);
                self.block("D3D12 discovery candidate bound (64) exceeded");
            }
        }
    }

    fn retire_unused_candidates(&mut self) {
        let key = |identity: DsvIdentity| {
            (
                identity.resource.0,
                identity.descriptor_generation,
                identity.resource_epoch,
            )
        };
        // This set contains only existing candidate keys, so it cannot exceed
        // the same 64-entry bound. Protect every current descriptor, including
        // aliases whose key is represented by a different descriptor handle.
        let mut referenced = BTreeSet::new();
        let mut protect = |identity: DsvIdentity| {
            let key = key(identity);
            if self.candidates.contains_key(&key) {
                referenced.insert(key);
            }
        };
        for descriptor in self.descriptors.values() {
            if let Some(identity) = descriptor.identity
                && identity.resource_epoch == self.resource_epoch
            {
                protect(identity);
            }
        }
        // Execute does not consume a recorded list: it may be resubmitted
        // before Reset. Retain its exact Bind/Clear identities even after a
        // submission, and keep the currently recorded bind as well.
        for list in self.lists.values() {
            if let Some(identity) = list.bound_depth {
                protect(identity);
            }
            for event in &list.events {
                match event {
                    ListEvent::BindDepth(Some(identity))
                    | ListEvent::ClearDepth {
                        identity: Some(identity),
                        ..
                    } => protect(*identity),
                    _ => {}
                }
            }
        }
        self.candidates.retain(|key, candidate| {
            referenced.contains(key)
                || candidate.pending_clears != 0
                || candidate.pending_same_list_capture_eligible_clears != 0
        });
        // Only diagnostic history was retired. Descriptor/resource identities,
        // selected tokens, pending list events and sticky structural faults
        // remain untouched; stale work must still fail at execution/capture.
    }

    fn resolve_handle(&self, handle: u64) -> Option<DsvIdentity> {
        self.descriptors
            .get(&handle)
            .and_then(|record| record.identity)
    }

    fn dsv_flags_for(&self, identity: DsvIdentity) -> Option<i32> {
        self.descriptors
            .get(&identity.handle.0)
            .filter(|record| record.identity == Some(identity))
            .and_then(|record| record.dsv_flags)
    }

    fn release_resource(&mut self, object: usize) {
        let Some(resource) = self.resources.remove(&object) else {
            return;
        };
        for descriptor in self.descriptors.values_mut() {
            if descriptor
                .identity
                .is_some_and(|identity| identity.resource == resource.id)
            {
                descriptor.identity = None;
            }
        }
    }

    fn record_event(&mut self, list: usize, event: ListEvent, label: &str) {
        let Some(record) = self.lists.get_mut(&list) else {
            self.block(format!("{label} arrived for an unknown command list"));
            return;
        };
        record.events.push(event);
    }

    fn record_bound_depth(&mut self, list: usize, identity: Option<DsvIdentity>) {
        let Some(record) = self.lists.get_mut(&list) else {
            self.block("OMSetRenderTargets arrived for an unknown command list");
            return;
        };
        record.bound_depth = identity;
    }

    fn record_transition_state(&mut self, list: usize, transition: RawTransition) {
        let Some(resource) = self.resources.get(&transition.resource).copied() else {
            return;
        };
        let Some(record) = self.lists.get_mut(&list) else {
            self.block("ResourceBarrier arrived for an unknown command list");
            return;
        };
        let scope = resource.transition_scope(transition.subresource);
        if scope == DepthTransitionScope::Stencil {
            return;
        }
        if transition.split || scope == DepthTransitionScope::Unsupported {
            record.recording_states.remove(&resource.id);
            return;
        }
        let before = transition.before.0;
        let after = transition.after.0;
        if let Some(actual) = record.recording_states.get(&resource.id)
            && *actual != before
        {
            record.recording_states.remove(&resource.id);
            return;
        }
        record.recording_states.insert(resource.id, after);
    }

    fn capture_source_for_clear(
        &self,
        list: usize,
        identity: DsvIdentity,
    ) -> Result<usize, String> {
        if !self.identity_is_current(identity) {
            return Err("capture clear used a stale DSV identity".into());
        }
        let record = self
            .lists
            .get(&list)
            .ok_or_else(|| "capture clear used an unknown command list".to_string())?;
        if record.kind != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return Err("capture clear command list is not DIRECT".into());
        }
        // ID3D12GraphicsCommandList::ClearDepthStencilView addresses a DSV
        // directly; D3D12 does not require that descriptor to be installed by
        // OMSetRenderTargets first. CP2077 intentionally clears before the OM
        // bind. Requiring a bind rejects a valid, exact descriptor identity.
        if record.recording_states.get(&identity.resource)
            != Some(&D3D12_RESOURCE_STATE_DEPTH_WRITE.0)
        {
            return Err("capture clear has no same-list proof of DEPTH_WRITE state".into());
        }
        self.resources
            .iter()
            .find_map(|(raw, resource)| (resource.id == identity.resource).then_some(*raw))
            .ok_or_else(|| "capture clear resource identity has no live COM object".into())
    }

    fn reset_list(&mut self, list: usize) {
        let Some(record) = self.lists.get_mut(&list) else {
            self.block("Reset arrived for an unknown command list");
            return;
        };
        record.generation = record.generation.saturating_add(1);
        record.events.clear();
        record.submitted_queue = None;
        record.bound_depth = None;
        record.recording_states.clear();
    }

    fn execute(&mut self, queue: usize, lists: &[*mut c_void]) {
        self.execute_command_lists_calls += 1;
        let Some(queue_record) = self.queues.get(&queue).copied() else {
            self.block("ExecuteCommandLists used an unknown queue");
            return;
        };
        if queue_record.kind != D3D12_COMMAND_LIST_TYPE_DIRECT {
            // Ordinary COMPUTE/COPY submissions cannot supply this depth candidate.
            // Keep the rejection visible without poisoning an unrelated DIRECT capture.
            self.reject_candidate(format!("queue {:?} is not a DIRECT queue", queue_record.id));
            return;
        }

        for list_pointer in lists {
            let list_key = *list_pointer as usize;
            let Some(mut list_record) = self.lists.remove(&list_key) else {
                self.block(format!(
                    "queue {:?} submitted an unknown command list",
                    queue_record.id
                ));
                continue;
            };
            self.executed_lists += 1;
            if list_record.kind != D3D12_COMMAND_LIST_TYPE_DIRECT {
                self.block(format!("command list {} is not DIRECT", list_record.id));
            }
            if let Some(previous) = list_record.submitted_queue
                && previous != queue_record.id
            {
                self.block(format!(
                    "command list {} moved from queue {:?} to {:?}",
                    list_record.id, previous, queue_record.id
                ));
            }
            list_record.submitted_queue = Some(queue_record.id);
            self.apply_list_events(
                list_key,
                list_record.id,
                queue_record.id,
                &list_record.events,
            );
            self.lists.insert(list_key, list_record);
        }
    }

    fn apply_list_events(
        &mut self,
        list: usize,
        list_id: u64,
        queue: QueueId,
        events: &[ListEvent],
    ) {
        let mut bound_depth = None;

        for event in events {
            match *event {
                ListEvent::Transition(transition) => self.apply_transition(list_id, transition),
                ListEvent::BindDepth(identity) => {
                    if let Some(identity) = identity
                        && !self.identity_is_current(identity)
                    {
                        self.block(format!(
                            "command list {list_id} bound a stale DSV descriptor"
                        ));
                        bound_depth = None;
                    } else {
                        bound_depth = identity;
                        if let Some(identity) = identity {
                            let candidate_key = (
                                identity.resource.0,
                                identity.descriptor_generation,
                                identity.resource_epoch,
                            );
                            if let Some(candidate) = self.candidates.get_mut(&candidate_key) {
                                candidate.bind_calls = candidate.bind_calls.saturating_add(1);
                            }
                        }
                    }
                }
                ListEvent::ClearDepth {
                    identity,
                    flags,
                    depth_value_bits,
                    stencil,
                    rect_count,
                    same_list_capture_eligible,
                } => {
                    let Some(identity) = identity else {
                        self.block(format!(
                            "command list {list_id} cleared an unknown/null DSV"
                        ));
                        continue;
                    };
                    if !self.identity_is_current(identity) {
                        self.block(format!(
                            "command list {list_id} cleared a stale DSV descriptor"
                        ));
                        continue;
                    }
                    let candidate_key = (
                        identity.resource.0,
                        identity.descriptor_generation,
                        identity.resource_epoch,
                    );
                    let raw_state_before_clear = self
                        .resources
                        .values()
                        .find(|record| record.id == identity.resource)
                        .and_then(|record| record.raw_state);
                    if let Some(candidate) = self.candidates.get_mut(&candidate_key) {
                        candidate.clear_calls = candidate.clear_calls.saturating_add(1);
                        if same_list_capture_eligible {
                            candidate.same_list_capture_eligible_clear_calls = candidate
                                .same_list_capture_eligible_clear_calls
                                .saturating_add(1);
                            candidate.pending_same_list_capture_eligible_clears = candidate
                                .pending_same_list_capture_eligible_clears
                                .saturating_add(1);
                        }
                        candidate.execute_calls = candidate.execute_calls.saturating_add(1);
                        if bound_depth == Some(identity) {
                            candidate.clear_while_bound_calls =
                                candidate.clear_while_bound_calls.saturating_add(1);
                        }
                        *candidate
                            .clear_parameters
                            .entry((flags, depth_value_bits, stencil, rect_count))
                            .or_default() += 1;
                        *candidate
                            .raw_states_before_clear
                            .entry(raw_state_before_clear)
                            .or_default() += 1;
                        candidate.distinct_lists.insert(list);
                        candidate.distinct_queues.insert(queue.0);
                        candidate.pending_clears = candidate.pending_clears.saturating_add(1);
                    }
                    if same_list_capture_eligible {
                        // The hook-time check used this exact descriptor identity
                        // and the exact command-list-local raw state immediately
                        // before ClearDepthStencilView. Production capture remains
                        // separately armed for one operator-pinned identity.
                        self.eligible_pre_clear_snapshots += 1;
                        if let Some(candidate) = self.candidates.get_mut(&candidate_key) {
                            candidate.eligible_clear_calls =
                                candidate.eligible_clear_calls.saturating_add(1);
                        }
                    } else {
                        self.reject_candidate(format!(
                            "command list {list_id} clear had no proven DEPTH_WRITE state in the same command list"
                        ));
                    }
                }
            }
        }
    }

    fn note_present(&mut self) {
        self.present_calls = self.present_calls.saturating_add(1);
        for candidate in self.candidates.values_mut() {
            if candidate.pending_clears > 0 {
                candidate.present_intervals_with_clear =
                    candidate.present_intervals_with_clear.saturating_add(1);
                match candidate.pending_clears {
                    1 => {
                        candidate.present_intervals_with_one_clear =
                            candidate.present_intervals_with_one_clear.saturating_add(1);
                    }
                    _ => {
                        candidate.present_intervals_with_multiple_clears = candidate
                            .present_intervals_with_multiple_clears
                            .saturating_add(1);
                    }
                }
                candidate.max_clears_per_present_interval = candidate
                    .max_clears_per_present_interval
                    .max(candidate.pending_clears);
                candidate
                    .first_present_call_with_clear
                    .get_or_insert(self.present_calls);
                candidate.last_present_call_with_clear = Some(self.present_calls);
                if candidate.pending_same_list_capture_eligible_clears > 0 {
                    candidate.present_intervals_with_same_list_capture_eligible_clear = candidate
                        .present_intervals_with_same_list_capture_eligible_clear
                        .saturating_add(1);
                }
                candidate.pending_clears = 0;
                candidate.pending_same_list_capture_eligible_clears = 0;
            }
        }
    }

    fn candidate_observations(&self) -> Vec<D3d12DepthCandidateObservation> {
        self.candidates
            .values()
            .map(|candidate| D3d12DepthCandidateObservation {
                candidate_id: format!(
                    "dsv-r{}-g{}-e{}",
                    candidate.identity.resource.0,
                    candidate.identity.descriptor_generation,
                    candidate.identity.resource_epoch
                ),
                descriptor_generation: candidate.identity.descriptor_generation,
                resource_epoch: candidate.identity.resource_epoch,
                width: candidate.metadata.width,
                height: candidate.metadata.height,
                sample_count: candidate.metadata.sample_count,
                sample_quality: candidate.metadata.sample_quality,
                resource_format: candidate.metadata.resource_format,
                dsv_format: candidate.metadata.dsv_format,
                clear_calls: candidate.clear_calls,
                eligible_clear_calls: candidate.eligible_clear_calls,
                same_list_capture_eligible_clear_calls: candidate
                    .same_list_capture_eligible_clear_calls,
                execute_calls: candidate.execute_calls,
                present_intervals_with_clear: candidate.present_intervals_with_clear,
                present_intervals_with_one_clear: candidate.present_intervals_with_one_clear,
                present_intervals_with_multiple_clears: candidate
                    .present_intervals_with_multiple_clears,
                max_clears_per_present_interval: candidate.max_clears_per_present_interval,
                first_present_call_with_clear: candidate.first_present_call_with_clear,
                last_present_call_with_clear: candidate.last_present_call_with_clear,
                pending_clear_calls: candidate.pending_clears,
                present_intervals_with_same_list_capture_eligible_clear: candidate
                    .present_intervals_with_same_list_capture_eligible_clear,
                bind_calls: candidate.bind_calls,
                clear_while_bound_calls: candidate.clear_while_bound_calls,
                clear_parameters: candidate
                    .clear_parameters
                    .iter()
                    .map(
                        |(&(clear_flags, depth_value_bits, stencil, rect_count), &calls)| {
                            D3d12ClearParameterObservation {
                                clear_flags,
                                depth_value_bits,
                                stencil,
                                rect_count,
                                calls,
                            }
                        },
                    )
                    .collect(),
                raw_states_before_clear: candidate
                    .raw_states_before_clear
                    .iter()
                    .map(|(&state, &calls)| D3d12RawStateObservation { state, calls })
                    .collect(),
                transition_pairs: candidate
                    .transition_pairs
                    .iter()
                    .map(
                        |(&(state_before, state_after, subresource, split), &calls)| {
                            D3d12TransitionPairObservation {
                                state_before,
                                state_after,
                                subresource,
                                split,
                                calls,
                            }
                        },
                    )
                    .collect(),
                distinct_command_lists: candidate.distinct_lists.len(),
                distinct_direct_queues: candidate.distinct_queues.len(),
                current: self.identity_is_current(candidate.identity),
            })
            .collect()
    }

    fn identity_is_current(&self, identity: DsvIdentity) -> bool {
        identity.resource_epoch == self.resource_epoch
            && self
                .descriptors
                .get(&identity.handle.0)
                .is_some_and(|record| record.identity == Some(identity))
    }

    fn apply_transition(&mut self, list_id: u64, transition: RawTransition) {
        let Some(resource_record) = self.resources.get(&transition.resource).copied() else {
            // Most barriers are unrelated render resources. Only DSV resources
            // enter this map, so ignoring these is required to avoid false gates.
            return;
        };
        let resource_id = resource_record.id;
        for candidate in self
            .candidates
            .values_mut()
            .filter(|candidate| candidate.identity.resource == resource_id)
        {
            *candidate
                .transition_pairs
                .entry((
                    transition.before.0,
                    transition.after.0,
                    transition.subresource,
                    transition.split,
                ))
                .or_default() += 1;
        }
        let scope = resource_record.transition_scope(transition.subresource);
        if scope == DepthTransitionScope::Stencil {
            return;
        }
        if transition.split {
            if let Some(resource) = self.resources.get_mut(&transition.resource) {
                resource.raw_state = None;
            }
            self.reject_candidate(format!(
                "command list {list_id} used an unsupported split barrier for a DSV resource"
            ));
            return;
        }
        if scope == DepthTransitionScope::Unsupported {
            if let Some(resource) = self.resources.get_mut(&transition.resource) {
                resource.raw_state = None;
            }
            self.reject_candidate(format!(
                "command list {list_id} used per-subresource DSV state tracking"
            ));
            return;
        }
        if let Some(resource) = self.resources.get_mut(&transition.resource) {
            resource.raw_state = Some(transition.after.0);
        }
        let resource = self
            .resources
            .get_mut(&transition.resource)
            .expect("DSV resource disappeared while applying one barrier");
        // Global state is diagnostic only; all capture admission uses the exact
        // command-list-local raw state at hook time. Preserve a narrow legacy
        // summary when representable, but do not treat legal composed read
        // states as a global fatal blocker.
        resource.state = supported_state(transition.after);
    }
}

/// A compact report suitable for synthetic smoke assertions and readiness UI.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct D3d12ObservationSnapshot {
    pub resource_epoch: u64,
    pub queue_count: usize,
    pub command_list_count: usize,
    pub depth_resource_count: usize,
    pub descriptor_count: usize,
    pub create_command_queue_calls: u64,
    pub create_command_list_calls: u64,
    pub create_dsv_calls: u64,
    pub resource_barrier_calls: u64,
    pub om_set_render_targets_calls: u64,
    pub clear_depth_calls: u64,
    pub execute_command_lists_calls: u64,
    pub executed_lists: u64,
    pub eligible_pre_clear_snapshots: u64,
    pub descriptor_overwrites: u64,
    pub present_calls: u64,
    pub candidates_dropped: u64,
    pub depth_candidates: Vec<D3d12DepthCandidateObservation>,
    pub registered_swapchain_count: usize,
    pub capture_safe: bool,
    pub snapshot_copy_implemented: bool,
    pub fence_readback_implemented: bool,
    pub capture_armed: bool,
    pub capture_in_flight: usize,
    pub capture_dropped_pool_full: u64,
    pub capture_error: Option<String>,
    pub blocker_counts: Vec<D3d12BlockerCountObservation>,
    pub blockers: Vec<String>,
}

/// Owns the dummy device whose shared runtime vtable was used to arm early
/// device hooks. The launcher/probe must retain this value until shutdown.
/// Hook installation proves only that early observation is armed; it is not a
/// depth-ready signal.
pub struct D3d12Bootstrap {
    backend: Arc<D3d12ObservationBackend>,
    _bootstrap_device: ID3D12Device,
    _bootstrap_factory: IDXGIFactory2,
}

impl D3d12Bootstrap {
    /// Creates a device on the system default adapter before the game resumes,
    /// installs device-level hooks on its runtime vtable, and keeps that device
    /// alive. A synthetic smoke must still prove that a separately created
    /// device shares those hooked slots on the target runtime.
    pub fn default_adapter() -> Result<Self, String> {
        let mut device = None;
        unsafe { D3D12CreateDevice(None::<&IUnknown>, D3D_FEATURE_LEVEL_12_0, &mut device) }
            .map_err(|error| format!("D3D12 bootstrap device creation failed: {error}"))?;
        let device: ID3D12Device =
            device.ok_or_else(|| "D3D12 bootstrap returned no device".to_string())?;
        let backend = D3d12ObservationBackend::new();
        unsafe { backend.install(Interface::as_raw(&device)) }?;
        let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }
            .map_err(|error| format!("DXGI bootstrap factory creation failed: {error}"))?;
        unsafe { backend.install_factory(Interface::as_raw(&factory)) }?;
        Ok(Self {
            backend,
            _bootstrap_device: device,
            _bootstrap_factory: factory,
        })
    }

    #[must_use]
    pub fn backend(&self) -> &Arc<D3d12ObservationBackend> {
        &self.backend
    }
}

/// One process-wide observation backend. Hooks forward every call to its exact
/// original target and only mutate mutex-protected bookkeeping afterward.
pub struct D3d12ObservationBackend {
    camera_execute_prepare_timing: crate::hook_timing::TimingCounter,
    camera_execute_prepare_details: crate::execute_prepare::PrepareCounters,
    camera_execute_complete_timing: crate::hook_timing::TimingCounter,
    camera_selected_draw_timing: crate::hook_timing::TimingCounter,
    camera_selected_draw_lock_wait_timing: crate::hook_timing::TimingCounter,
    camera_selected_draw_locked_timing: crate::hook_timing::TimingCounter,
    camera_scissor_timing: crate::hook_timing::TimingCounter,
    original_target_miss_timing: crate::hook_timing::TimingCounter,
    descriptor_write_timing: crate::hook_timing::TimingCounter,
    descriptor_invalidate_timing: crate::hook_timing::TimingCounter,
    descriptor_copy_timing: crate::hook_timing::TimingCounter,
    resource_map_timing: crate::hook_timing::TimingCounter,
    resource_unmap_timing: crate::hook_timing::TimingCounter,
    buffer_copy_timing: crate::hook_timing::TimingCounter,
    observation_snapshot_timing: crate::hook_timing::TimingCounter,
    camera_snapshot_timing: crate::hook_timing::TimingCounter,
    camera_snapshot_lock_wait_timing: crate::hook_timing::TimingCounter,
    camera_snapshot_copy_timing: crate::hook_timing::TimingCounter,
    camera_snapshot_admission_timing: crate::hook_timing::TimingCounter,
    camera_present_summary_timing: crate::hook_timing::TimingCounter,
    camera_present_lock_wait_timing: crate::hook_timing::TimingCounter,
    camera_execute_prepare_lock_immediate: AtomicU64,
    camera_execute_prepare_lock_contended: AtomicU64,
    camera_execute_complete_lock_immediate: AtomicU64,
    camera_execute_complete_lock_contended: AtomicU64,
    camera_execute_with_prepared_copy_shadows: AtomicU64,
    camera_execute_prepared_copy_shadows: AtomicU64,
    camera_execute_with_depth_copy_proof: AtomicU64,
    camera_execute_with_camera_work: AtomicU64,
    camera_execute_skipped_no_camera_work: AtomicU64,
    production_camera_take_lock_immediate: AtomicU64,
    production_camera_take_lock_contended: AtomicU64,
    state: Mutex<ObservationState>,
    hooks: Mutex<HookRegistry>,
    runtime_observation_active: AtomicBool,
    original_targets: OriginalTargetCache,
    capture: Mutex<Option<D3d12CaptureRing>>,
    capture_armed: AtomicBool,
    capture_exact_transition_enabled: AtomicBool,
    capture_transition_before: AtomicI32,
    capture_transition_after: AtomicI32,
    capture_transition_subresource: AtomicU32,
    capture_transition_flags: AtomicI32,
    capture_fallback_draw: Mutex<Option<depth_lab::D3d12FallbackDrawIdentity>>,
    capture_association_fault: AtomicBool,
    swapchains: Mutex<BTreeMap<usize, SwapchainRecord>>,
    // Hot descriptor hooks never acquire render_camera. Cold paths lock it first.
    cbv_descriptor_store: Arc<CbvDescriptorStore>,
    render_camera: Mutex<RenderCameraTracker>,
    // Creation/release metadata is read separately from mutable camera observations so
    // unrelated Draw callbacks never contend for the camera/proof lock.
    root_fingerprints: RwLock<BTreeMap<usize, String>>,
    // Published once from the validated CameraBinding and retained across runs in this process.
    // Draw callbacks use it with hot-list atomics before allocating bindings or taking the global
    // render-camera mutex. The tracker repeats the same check after locking as a fail-closed guard.
    production_draw_filter: OnceLock<ProductionDrawFilter>,
    pso_registry: pipeline_depth::PsoRegistry,
    // CP2077 creates a material share of transient buffers through newer device
    // revisions whose process-wide creation hooks are too hot for a safe early
    // bootstrap.  Keep only upload buffers that actually participate in an
    // observed CopyBufferRegion persistently mapped. D3D12 explicitly supports
    // nested persistent Map calls; retaining the COM reference makes the CPU
    // pointer valid until process teardown.
    observer_upload_mappings: Mutex<BTreeMap<usize, ID3D12Resource>>,
    render_camera_contention_drops: AtomicU64,
    /// Descriptor copies that named no tracked CBV on either side and changed
    /// nothing after visiting their shards. Failed lock acquisitions do not
    /// count as untracked copies.
    descriptor_copy_untracked: AtomicU64,
    /// Descriptors actually moved across all copies. Divided by
    /// `descriptor_copy.calls` it gives the batch size, which is what the
    /// per-call cost scales with: an offline sweep on the capture host measured
    /// a flat ~344 ns per descriptor, so the 2026-09-10 run's 39.8 us average
    /// back-solves to about 116 per call. Recording it makes the next run
    /// measure that number instead of inferring it.
    descriptor_copy_descriptors: AtomicU64,
    /// Splits the `descriptor_copy` wall time into the three things it actually
    /// does, because the single total could not be attributed. The 2026-09-11
    /// run measured 38.499 us per call at a *measured* 6.165 descriptors per
    /// call - about 182 ns of hash lookups, or 0.47% of the total. Whatever
    /// spends the other 99.5% has to be one of these three: building the handle
    /// ranges before the guard, waiting for the guard, or the work under it.
    descriptor_copy_prepare_timing: crate::hook_timing::TimingCounter,
    descriptor_copy_lock_wait_timing: crate::hook_timing::TimingCounter,
    descriptor_copy_apply_timing: crate::hook_timing::TimingCounter,
    /// Batch-size histogram. The mean alone hid a 19x error once already; a
    /// heavy tail and a flat distribution with the same mean need different
    /// fixes. Buckets: 1, 2-8, 9-32, 33-128, 129-512, 513-4096.
    descriptor_copy_batch_buckets: [AtomicU64; 6],
    /// Distinct shards named by source UNION destination, counted once per copy.
    /// Empty copies name no shards and increment no bucket.
    descriptor_copy_shard_buckets: [AtomicU64; 5],
    /// Descriptor heaps released while tracked. Each one now costs a full scan
    /// of `cbv_descriptors` instead of an ordered range lookup - 2.5 ms at the
    /// 280,588 entries CP2077 holds. The map growing that large only proves
    /// heaps *holding tracked CBVs* are rarely released; a transient heap with
    /// none would still pay the scan, so measure the rate rather than assume it.
    descriptor_heap_releases: AtomicU64,
    // Existing exact production/KCD2 quota, replenished at Present and spent after exact-source
    // filtering. Sparse discovery uses the recording-owned HotListSlot counter instead.
    render_camera_draw_budget: AtomicU64,
    camera_discovery_draw_sampled: AtomicU64,
    camera_discovery_list_budget_skipped: AtomicU64,
    render_camera_production: AtomicBool,
    hot_lists: HotListRegistry,
    selected_depth_token: AtomicU64,
    depth_order_diagnostics: AtomicBool,
    bound_holstered_camera: AtomicBool,
    diagnostic_depth_resource_raw: AtomicUsize,
    selected_depth_next_token: AtomicU64,
    selected_depth_handle: AtomicU64,
    selected_depth_descriptor_generation: AtomicU64,
    selected_depth_resource: AtomicU64,
    selected_depth_resource_epoch: AtomicU64,
    direct_root_cbv_calls: AtomicU64,
    descriptor_table_calls: AtomicU64,
}

static ACTIVE_BACKEND: OnceLock<Arc<D3d12ObservationBackend>> = OnceLock::new();

impl D3d12ObservationBackend {
    #[must_use]
    pub fn new() -> Arc<Self> {
        let cbv_descriptor_store = Arc::new(CbvDescriptorStore::new());
        Arc::new(Self {
            camera_execute_prepare_timing: Default::default(),
            camera_execute_prepare_details: Default::default(),
            camera_execute_complete_timing: Default::default(),
            camera_selected_draw_timing: Default::default(),
            camera_selected_draw_lock_wait_timing: Default::default(),
            camera_selected_draw_locked_timing: Default::default(),
            camera_scissor_timing: Default::default(),
            original_target_miss_timing: Default::default(),
            descriptor_write_timing: Default::default(),
            descriptor_invalidate_timing: Default::default(),
            descriptor_copy_timing: Default::default(),
            resource_map_timing: Default::default(),
            resource_unmap_timing: Default::default(),
            buffer_copy_timing: Default::default(),
            observation_snapshot_timing: Default::default(),
            camera_snapshot_timing: Default::default(),
            camera_snapshot_lock_wait_timing: Default::default(),
            camera_snapshot_copy_timing: Default::default(),
            camera_snapshot_admission_timing: Default::default(),
            camera_present_summary_timing: Default::default(),
            camera_present_lock_wait_timing: Default::default(),
            camera_execute_prepare_lock_immediate: AtomicU64::new(0),
            camera_execute_prepare_lock_contended: AtomicU64::new(0),
            camera_execute_complete_lock_immediate: AtomicU64::new(0),
            camera_execute_complete_lock_contended: AtomicU64::new(0),
            camera_execute_with_prepared_copy_shadows: AtomicU64::new(0),
            camera_execute_prepared_copy_shadows: AtomicU64::new(0),
            camera_execute_with_depth_copy_proof: AtomicU64::new(0),
            camera_execute_with_camera_work: AtomicU64::new(0),
            camera_execute_skipped_no_camera_work: AtomicU64::new(0),
            production_camera_take_lock_immediate: AtomicU64::new(0),
            production_camera_take_lock_contended: AtomicU64::new(0),
            state: Mutex::new(ObservationState::new()),
            hooks: Mutex::new(HookRegistry::new()),
            runtime_observation_active: AtomicBool::new(false),
            original_targets: OriginalTargetCache::default(),
            capture: Mutex::new(None),
            capture_armed: AtomicBool::new(false),
            capture_exact_transition_enabled: AtomicBool::new(false),
            capture_transition_before: AtomicI32::new(0),
            capture_transition_after: AtomicI32::new(0),
            capture_transition_subresource: AtomicU32::new(0),
            capture_transition_flags: AtomicI32::new(0),
            capture_fallback_draw: Mutex::new(None),
            capture_association_fault: AtomicBool::new(false),
            swapchains: Mutex::new(BTreeMap::new()),
            // CP2077's selected D32 target is 1280x720 and PR #727 fixes vertical FOV to 75°.
            // These are discovery filters only; they never author a production binding.
            render_camera: Mutex::new(RenderCameraTracker::with_descriptor_store(
                1920,
                1080,
                75.0,
                Arc::clone(&cbv_descriptor_store),
            )),
            cbv_descriptor_store,
            root_fingerprints: RwLock::new(BTreeMap::new()),
            production_draw_filter: OnceLock::new(),
            pso_registry: Default::default(),
            observer_upload_mappings: Mutex::new(BTreeMap::new()),
            render_camera_contention_drops: AtomicU64::new(0),
            descriptor_copy_untracked: AtomicU64::new(0),
            descriptor_copy_descriptors: AtomicU64::new(0),
            descriptor_copy_prepare_timing: Default::default(),
            descriptor_copy_lock_wait_timing: Default::default(),
            descriptor_copy_apply_timing: Default::default(),
            descriptor_copy_batch_buckets: Default::default(),
            descriptor_copy_shard_buckets: Default::default(),
            descriptor_heap_releases: AtomicU64::new(0),
            render_camera_draw_budget: AtomicU64::new(1),
            camera_discovery_draw_sampled: AtomicU64::new(0),
            camera_discovery_list_budget_skipped: AtomicU64::new(0),
            render_camera_production: AtomicBool::new(false),
            hot_lists: HotListRegistry::new(),
            selected_depth_token: AtomicU64::new(0),
            depth_order_diagnostics: AtomicBool::new(false),
            bound_holstered_camera: AtomicBool::new(false),
            diagnostic_depth_resource_raw: AtomicUsize::new(0),
            selected_depth_next_token: AtomicU64::new(1),
            selected_depth_handle: AtomicU64::new(0),
            selected_depth_descriptor_generation: AtomicU64::new(0),
            selected_depth_resource: AtomicU64::new(0),
            selected_depth_resource_epoch: AtomicU64::new(0),
            direct_root_cbv_calls: AtomicU64::new(0),
            descriptor_table_calls: AtomicU64::new(0),
        })
    }

    /// Installs the device discovery hooks. Queue/list hooks are installed on
    /// successful creation by the corresponding ABI trampolines.
    ///
    /// # Safety
    ///
    /// `device` must be a live `ID3D12Device` pointer and remain live for the
    /// process lifetime. The backend intentionally does not support unloading.
    pub unsafe fn install(self: &Arc<Self>, device: *mut c_void) -> Result<(), String> {
        ACTIVE_BACKEND
            .set(Arc::clone(self))
            .map_err(|_| "a D3D12 observation backend is already active".to_string())?;
        unsafe {
            self.install_method(
                device,
                d3d12_abi::device::CREATE_COMMAND_QUEUE,
                HookMethod::CreateCommandQueue,
                hook_create_command_queue as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_COMMAND_LIST,
                HookMethod::CreateCommandList,
                hook_create_command_list as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_DEPTH_STENCIL_VIEW,
                HookMethod::CreateDepthStencilView,
                hook_create_depth_stencil_view as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_DESCRIPTOR_HEAP,
                HookMethod::CreateDescriptorHeap,
                hook_create_descriptor_heap as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_ROOT_SIGNATURE,
                HookMethod::CreateRootSignature,
                hook_create_root_signature as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_CONSTANT_BUFFER_VIEW,
                HookMethod::CreateConstantBufferView,
                hook_create_constant_buffer_view as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_SHADER_RESOURCE_VIEW,
                HookMethod::CreateShaderResourceView,
                hook_create_shader_resource_view as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_UNORDERED_ACCESS_VIEW,
                HookMethod::CreateUnorderedAccessView,
                hook_create_unordered_access_view as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::COPY_DESCRIPTORS,
                HookMethod::CopyDescriptors,
                hook_copy_descriptors as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::COPY_DESCRIPTORS_SIMPLE,
                HookMethod::CopyDescriptorsSimple,
                hook_copy_descriptors_simple as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_COMMITTED_RESOURCE,
                HookMethod::CreateCommittedResource,
                hook_create_committed_resource as *const () as usize,
            )?;
            self.install_method(
                device,
                d3d12_abi::device::CREATE_PLACED_RESOURCE,
                HookMethod::CreatePlacedResource,
                hook_create_placed_resource as *const () as usize,
            )?;
        }
        unsafe {
            self.install_pipeline_devices(device);
        }
        // Do not hook the modern Create*Resource revisions here. In CP2077 they
        // are extremely hot during startup. CopyBufferRegion lazily registers
        // only the exact source/destination resources that can feed a selected
        // draw, which preserves the same evidence boundary without stalling the
        // game on millions of irrelevant creations.
        Ok(())
    }

    /// Installs the two factory entry points used by desktop D3D12 games. The
    /// successful factory call is the only trustworthy place to associate a
    /// swapchain with the DIRECT queue supplied as `pDevice`.
    ///
    /// # Safety
    ///
    /// `factory` must be a live IDXGIFactory2 pointer retained by the caller.
    unsafe fn install_factory(&self, factory: *mut c_void) -> Result<(), String> {
        unsafe {
            self.install_method(
                factory,
                factory_create_swapchain_index(),
                HookMethod::CreateSwapChain,
                hook_create_swapchain as *const () as usize,
            )?;
            self.install_method(
                factory,
                factory2_create_swapchain_for_hwnd_index(),
                HookMethod::CreateSwapChainForHwnd,
                hook_create_swapchain_for_hwnd as *const () as usize,
            )
        }
    }

    fn register_swapchain(
        &self,
        queue_candidate: *mut c_void,
        swapchain: *mut c_void,
    ) -> Result<(), String> {
        if queue_candidate.is_null() || swapchain.is_null() {
            return Err("DXGI factory returned a null queue/swapchain association".into());
        }
        let unknown = unsafe { IUnknown::from_raw_borrowed(&queue_candidate) }
            .ok_or("DXGI factory pDevice is not a live COM object")?;
        let queue: ID3D12CommandQueue = unknown
            .cast()
            .map_err(|_| "DXGI swapchain pDevice is not an ID3D12CommandQueue".to_string())?;
        if unsafe { queue.GetDesc() }.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return Err("DXGI D3D12 swapchain queue is not DIRECT".into());
        }
        let mut device = None;
        unsafe { queue.GetDevice(&mut device) }
            .map_err(|error| format!("D3D12 swapchain queue GetDevice failed: {error}"))?;
        let device: ID3D12Device =
            device.ok_or("D3D12 swapchain queue GetDevice returned no device")?;
        let key = swapchain as usize;
        let queue_id = self
            .state
            .lock()
            .map_err(|_| "D3D12 observation state mutex poisoned".to_string())?
            .queues
            .get(&(queue_candidate as usize))
            .filter(|record| record.kind == D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map(|record| record.id)
            .ok_or("DXGI swapchain queue has no exact observed DIRECT identity")?;
        let mut swapchains = self
            .swapchains
            .lock()
            .map_err(|_| "D3D12 swapchain registry mutex poisoned".to_string())?;
        if let Some(existing) = swapchains.get(&key) {
            if existing.queue.as_raw() != queue.as_raw()
                || existing.device.as_raw() != device.as_raw()
            {
                return Err("D3D12 swapchain pointer was reused with another queue/device".into());
            }
            return Ok(());
        }
        swapchains.insert(
            key,
            SwapchainRecord {
                queue,
                queue_id,
                device,
                watermark: None,
                present_calls: 0,
                resize_calls: 0,
                watermark_submitted: 0,
                watermark_dropped: 0,
                submitted_sequences: BTreeSet::new(),
            },
        );
        Ok(())
    }

    #[must_use]
    pub fn is_d3d12_swapchain(&self, swapchain: *mut c_void) -> bool {
        !swapchain.is_null()
            && self
                .swapchains
                .lock()
                .is_ok_and(|records| records.contains_key(&(swapchain as usize)))
    }

    pub fn set_runtime_observation_active(&self, active: bool) {
        self.runtime_observation_active
            .store(active, Ordering::Release);
    }

    #[must_use]
    pub fn runtime_observation_active(&self) -> bool {
        self.runtime_observation_active.load(Ordering::Acquire)
    }

    pub fn swapchain_device(&self, swapchain: *mut c_void) -> Result<ID3D12Device, String> {
        self.swapchains
            .lock()
            .map_err(|_| "D3D12 swapchain registry mutex poisoned".to_string())?
            .get(&(swapchain as usize))
            .map(|record| record.device.clone())
            .ok_or_else(|| "swapchain has no factory-observed D3D12 queue".to_string())
    }

    pub fn swapchain_queue(&self, swapchain: *mut c_void) -> Result<ID3D12CommandQueue, String> {
        self.swapchains
            .lock()
            .map_err(|_| "D3D12 swapchain registry poisoned")?
            .get(&(swapchain as usize))
            .map(|record| record.queue.clone())
            .ok_or_else(|| "swapchain has no factory-observed D3D12 queue".into())
    }

    /// Records one pre-Present watermark on the factory-associated DIRECT
    /// queue. Backbuffers are acquired only after the association is proven.
    pub fn before_present(
        &self,
        swapchain: *mut c_void,
        present_seq: u64,
        watermark_required: bool,
        origin_x: u32,
        origin_y: u32,
    ) -> Result<D3d12PresentResult, String> {
        // A missed Present merges two clear intervals and can make an invalid
        // DSV fingerprint look admissible. This critical section is small and
        // never overlaps the swapchain registry or a GPU call.
        self.state
            .lock()
            .map_err(|_| "D3D12 observation state mutex poisoned at Present".to_string())?
            .note_present();
        let mut records = self
            .swapchains
            .try_lock()
            .map_err(|_| "D3D12 swapchain registry busy/poisoned".to_string())?;
        let record = records
            .get_mut(&(swapchain as usize))
            .ok_or("swapchain has no factory-observed D3D12 queue")?;
        record.present_calls = record.present_calls.saturating_add(1);
        let present_qpc = current_qpc();
        // Exact production/KCD2 retain their existing global bound. Discovery sampling belongs
        // to each list recording: CPU Present cannot order draws recorded on other threads.
        if self.render_camera_production.load(Ordering::Acquire) || crate::kcd2_live_pose::enabled()
        {
            self.render_camera_draw_budget
                .store(4096, Ordering::Release);
        }
        if self.runtime_observation_active() {
            let _perf = crate::perf::scope(crate::perf::Section::Pose);
            match self.render_camera.lock() {
                Ok(mut tracker) => tracker.present(record.queue_id, present_seq, present_qpc),
                Err(_) => self.block("D3D12 render-camera tracker poisoned at Present"),
            }
        }
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        crate::cp2077_camera_diagnostics::sample_memory(present_seq, present_qpc);
        if !watermark_required {
            return Ok(D3d12PresentResult {
                present_calls: record.present_calls,
                watermark_submitted: false,
                watermark_dropped: false,
            });
        }
        let raw = swapchain;
        let _perf = crate::perf::scope(crate::perf::Section::Watermark);
        let borrowed = unsafe { IDXGISwapChain::from_raw_borrowed(&raw) }
            .ok_or("D3D12 swapchain pointer is invalid")?;
        let swapchain3: IDXGISwapChain3 = borrowed
            .cast()
            .map_err(|error| format!("D3D12 swapchain does not expose IDXGISwapChain3: {error}"))?;
        if record.watermark.is_none() {
            let desc = unsafe { borrowed.GetDesc() }
                .map_err(|error| format!("D3D12 swapchain GetDesc failed: {error}"))?;
            if !(2..=16).contains(&desc.BufferCount) {
                return Err(format!(
                    "unsupported D3D12 swapchain buffer count {}",
                    desc.BufferCount
                ));
            }
            let mut backbuffers = Vec::with_capacity(desc.BufferCount as usize);
            for index in 0..desc.BufferCount {
                let buffer: ID3D12Resource = unsafe { borrowed.GetBuffer(index) }
                    .map_err(|error| format!("D3D12 GetBuffer({index}) failed: {error}"))?;
                backbuffers.push(buffer);
            }
            record.watermark = Some(D3d12WatermarkRing::new(
                &record.device,
                &record.queue,
                &backbuffers,
                4,
            )?);
        }
        let index = unsafe { swapchain3.GetCurrentBackBufferIndex() } as usize;
        let encoded_seq = u16::try_from(present_seq & u64::from(u16::MAX))
            .map_err(|_| "watermark sequence narrowing failed".to_string())?;
        let outcome = record
            .watermark
            .as_mut()
            .ok_or("D3D12 watermark ring disappeared")?
            .write(index, encoded_seq, origin_x, origin_y)?;
        let (watermark_submitted, watermark_dropped) = match outcome {
            WatermarkWriteResult::Submitted(_) => {
                record.watermark_submitted = record.watermark_submitted.saturating_add(1);
                record.submitted_sequences.insert(present_seq);
                while record.submitted_sequences.len() > 64 {
                    let Some(first) = record.submitted_sequences.first().copied() else {
                        break;
                    };
                    record.submitted_sequences.remove(&first);
                }
                (true, false)
            }
            WatermarkWriteResult::DroppedRingBusy => {
                record.watermark_dropped = record.watermark_dropped.saturating_add(1);
                (false, true)
            }
        };
        Ok(D3d12PresentResult {
            present_calls: record.present_calls,
            watermark_submitted,
            watermark_dropped,
        })
    }

    #[must_use]
    pub fn watermark_submitted_for(&self, swapchain: *mut c_void, present_seq: u64) -> bool {
        self.swapchains.lock().is_ok_and(|records| {
            records
                .get(&(swapchain as usize))
                .is_some_and(|record| record.submitted_sequences.contains(&present_seq))
        })
    }

    pub fn prepare_swapchain_resize(&self, swapchain: *mut c_void) -> Result<(), String> {
        let mut records = self
            .swapchains
            .lock()
            .map_err(|_| "D3D12 swapchain registry mutex poisoned".to_string())?;
        let record = records
            .get_mut(&(swapchain as usize))
            .ok_or("swapchain has no factory-observed D3D12 queue")?;
        if record
            .watermark
            .as_ref()
            .is_some_and(|watermark| !watermark.can_invalidate())
        {
            return Err("D3D12 ResizeBuffers raced an in-flight watermark".into());
        }
        record.watermark = None;
        record.submitted_sequences.clear();
        record.resize_calls = record.resize_calls.saturating_add(1);
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned".to_string())?;
        if let Some(capture) = capture.as_mut() {
            capture.invalidate("D3D12 ResizeBuffers invalidated the armed depth resource epoch");
            return Err("cannot resize while D3D12 depth capture is armed".into());
        }
        Ok(())
    }

    pub fn unregister_swapchain(&self, swapchain: *mut c_void) {
        if let Ok(mut records) = self.swapchains.lock() {
            records.remove(&(swapchain as usize));
        }
    }

    pub fn begin_depth_capture_interval(&self, present_seq: u64, requested: bool) {
        if self.capture_armed.load(Ordering::Acquire)
            && let Ok(mut capture) = self.capture.lock()
            && let Some(capture) = capture.as_mut()
        {
            capture.begin_present_interval(present_seq, requested);
        }
    }

    pub fn capture_is_armed(&self) -> bool {
        self.capture_armed.load(Ordering::Acquire)
    }

    /// StopRun holds the Present lifecycle lock so no callback can reopen a
    /// requested interval. Capture callbacks serialize here, not on that lock.
    /// Keep capture armed until existing commands and leases have drained.
    pub fn stop_depth_capture_production(&self) -> Result<(), String> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned while stopping production")?;
        if let Some(ring) = capture.as_mut() {
            ring.stop_production();
        } else if self.capture_armed.load(Ordering::Acquire) {
            return Err("D3D12 capture was armed without a ring while stopping production".into());
        }
        Ok(())
    }

    pub fn selected_capture_safe(&self) -> bool {
        self.selected_capture_health().is_ok()
    }

    fn selected_capture_health(&self) -> Result<(), String> {
        if !self.capture_is_armed() {
            return Err("selected depth capture is not armed".into());
        }
        if self.capture_association_fault.load(Ordering::Acquire) {
            return Err("selected depth lost its Execute association".into());
        }
        {
            let state = self
                .state
                .lock()
                .map_err(|_| "selected depth observation state is poisoned")?;
            if state.structural_failure {
                return Err(format!(
                    "selected depth observation has a structural failure; recorded blocker counts: {:?}",
                    state.blocker_counts
                ));
            }
            let identity = state
                .resolve_handle(self.selected_depth_handle.load(Ordering::Acquire))
                .ok_or("selected depth descriptor no longer resolves")?;
            if !state.identity_is_current(identity)
                || self.selected_depth_token_for(Some(identity)) == 0
            {
                return Err("selected depth descriptor/resource identity changed".into());
            }
        }
        let capture = self.capture.try_lock().map_err(|error| match error {
            std::sync::TryLockError::WouldBlock => {
                "selected depth health check capture mutex is busy"
            }
            std::sync::TryLockError::Poisoned(_) => "selected depth capture mutex is poisoned",
        })?;
        let capture = capture
            .as_ref()
            .ok_or("selected depth capture ring is absent")?;
        if let Some(error) = capture.fatal_error() {
            return Err(format!("selected depth capture ring failed: {error}"));
        }
        Ok(())
    }

    pub fn capture_slots_in_flight(&self) -> usize {
        self.capture.lock().map_or(usize::MAX, |capture| {
            capture.as_ref().map_or(0, D3d12CaptureRing::in_flight)
        })
    }

    pub fn capture_pool_full_count(&self) -> u64 {
        self.capture.lock().map_or(u64::MAX, |capture| {
            capture
                .as_ref()
                .map_or(0, D3d12CaptureRing::dropped_pool_full)
        })
    }

    fn capture_before_depth_write_exit(
        &self,
        list_ptr: *mut c_void,
        transition: &D3D12_RESOURCE_TRANSITION_BARRIER,
        flags: D3D12_RESOURCE_BARRIER_FLAGS,
        barrier_original: usize,
    ) {
        if !self.capture_armed.load(Ordering::Acquire)
            || !self.capture_transition_admitted(
                transition.StateBefore,
                transition.StateAfter,
                transition.Subresource,
                flags,
            )
        {
            return;
        }
        let resource_ptr = transition
            .pResource
            .as_ref()
            .map_or(std::ptr::null_mut(), |resource| resource.as_raw());
        let Some(list) = (unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&list_ptr) })
        else {
            self.block("ResourceBarrier list pointer is invalid during depth capture");
            return;
        };
        match self.capture.lock() {
            Ok(mut capture) => {
                // StopRun may have retired the ring after the lock-free admission.
                if !self.capture_armed.load(Ordering::Acquire) {
                    return;
                }
                let Some(capture) = capture.as_mut() else {
                    self.block("D3D12 capture was armed without a capture ring");
                    return;
                };
                let diagnostic = self
                    .depth_order_diagnostics
                    .load(Ordering::Acquire)
                    .then(|| capture.diagnostic_interval(resource_ptr))
                    .flatten();
                let result = unsafe {
                    capture.record_before_depth_write_exit(
                        list_ptr,
                        list,
                        barrier_original,
                        resource_ptr,
                        self.hot_lists.recorded_draw_count(list_ptr as usize),
                    )
                };
                if matches!(
                    result,
                    Ok(crate::d3d12_capture::RecordResult::Recorded { .. })
                ) {
                    // Draw ordering around the private depth copy is part of the integrity proof.
                    // A later otherwise-identical Draw must reach the tracker instead of being
                    // represented by a pre-copy observation.
                    self.hot_lists
                        .mark_production_depth_copy_recorded(list_ptr as usize);
                }
                if let Some((identity, interval_present_seq, requested, already_captured)) =
                    diagnostic
                {
                    use crate::d3d12_render_camera::depth_order::BoundaryKind;
                    let (label, capture_id) = match &result {
                        Ok(crate::d3d12_capture::RecordResult::Recorded { capture_id, .. }) => {
                            ("recorded", Some(*capture_id))
                        }
                        Ok(crate::d3d12_capture::RecordResult::NotSelected) => {
                            ("not_selected", None)
                        }
                        Ok(crate::d3d12_capture::RecordResult::DroppedPoolFull) => {
                            ("pool_full", None)
                        }
                        Err(_) => ("error", None),
                    };
                    // Camera snapshots never acquire capture: this matches Execute's lock order.
                    if let Ok(mut tracker) = self.render_camera.lock() {
                        tracker.record_depth_boundary(
                            list_ptr as usize,
                            identity,
                            self.hot_lists.recorded_draw_count(list_ptr as usize),
                            BoundaryKind::CopyCandidate {
                                interval_present_seq,
                                requested,
                                already_captured,
                                result: label,
                                capture_id,
                            },
                        );
                    }
                }
                if let Err(error) = result {
                    self.block(error);
                }
            }
            Err(_) => self.block("D3D12 capture mutex poisoned during depth-write exit"),
        }
    }

    fn capture_at_proven_fallback_pass_end(&self, list_ptr: *mut c_void) {
        if !self.capture_armed.load(Ordering::Acquire)
            || self
                .capture_fallback_draw
                .try_lock()
                .map_or(true, |fallback| fallback.is_none())
            || !self.hot_lists.fallback_draw_proven(list_ptr as usize)
        {
            return;
        }
        let Some(list) = (unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&list_ptr) })
        else {
            self.block("fallback pass-end list pointer is invalid during depth capture");
            return;
        };
        let Some(barrier_original) = self.original_target(
            list_ptr,
            d3d12_abi::graphics_command_list::RESOURCE_BARRIER,
            HookMethod::ResourceBarrier,
        ) else {
            self.block("fallback pass-end ResourceBarrier original target is missing");
            return;
        };
        match self.capture.lock() {
            Ok(mut capture) => {
                if !self.capture_armed.load(Ordering::Acquire) {
                    return;
                }
                let Some(capture) = capture.as_mut() else {
                    self.block("D3D12 fallback capture was armed without a capture ring");
                    return;
                };
                let result = unsafe {
                    capture.record_at_proven_pass_end(
                        list_ptr,
                        list,
                        barrier_original,
                        self.hot_lists.recorded_draw_count(list_ptr as usize),
                    )
                };
                if matches!(
                    result,
                    Ok(crate::d3d12_capture::RecordResult::Recorded { .. })
                ) {
                    self.hot_lists
                        .mark_production_depth_copy_recorded(list_ptr as usize);
                }
                if let Err(error) = result {
                    self.block(error);
                }
            }
            Err(_) => self.block("D3D12 capture mutex poisoned during fallback pass end"),
        }
    }

    fn capture_transition_admitted(
        &self,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
        subresource: u32,
        flags: D3D12_RESOURCE_BARRIER_FLAGS,
    ) -> bool {
        if !self
            .capture_exact_transition_enabled
            .load(Ordering::Acquire)
        {
            return legacy_capture_transition_admitted(before, after, subresource, flags);
        }
        let selected_subresource = self.capture_transition_subresource.load(Ordering::Relaxed);
        // Stable selection and the capture ring require a single mip/array slice
        // and copy depth plane 0. A whole-resource transition covers that plane
        // too; stencil-only transitions do not. Keep the exact states and flags.
        let covers_selected_depth = subresource == selected_subresource
            || (selected_subresource == 0
                && subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES);
        before.0 == self.capture_transition_before.load(Ordering::Relaxed)
            && after.0 == self.capture_transition_after.load(Ordering::Relaxed)
            && covers_selected_depth
            && flags.0 == self.capture_transition_flags.load(Ordering::Relaxed)
    }

    /// Starts a new resource-identity epoch. It is intentionally explicit:
    /// this observation layer cannot infer swapchain resize or COM address reuse.
    pub fn begin_resource_epoch(&self) -> Result<u64, String> {
        let capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned".to_string())?;
        if capture.is_some() {
            return Err("cannot advance resource epoch while depth capture is armed".into());
        }
        self.capture_association_fault
            .store(false, Ordering::Release);
        drop(capture);
        let mut state = self
            .state
            .lock()
            .map_err(|_| "D3D12 observation state mutex poisoned".to_string())?;
        if state.lists.values().any(|list| !list.events.is_empty()) {
            return Err(
                "cannot advance resource epoch while command-list events are pending".into(),
            );
        }
        if !state.lists.is_empty() || !state.queues.is_empty() {
            return Err(
                "cannot advance resource epoch while queue/list COM objects are live".into(),
            );
        }
        state.resource_epoch = state
            .resource_epoch
            .checked_add(1)
            .ok_or_else(|| "resource epoch overflow".to_string())?;
        state.resources.clear();
        state.descriptors.clear();
        Ok(state.resource_epoch)
    }

    #[must_use]
    pub fn snapshot(&self) -> D3d12ObservationSnapshot {
        let _timer = self.observation_snapshot_timing.start();
        // Do not hold observation state while waiting for the swapchain
        // registry. before_present() keeps the registry across watermark GPU
        // submission, whose Execute hook records into observation state.
        let registered_swapchain_count = self.swapchains.lock().map_or(0, |records| records.len());
        let mut snapshot = match self.state.lock() {
            Ok(state) => D3d12ObservationSnapshot {
                resource_epoch: state.resource_epoch,
                queue_count: state.queues.len(),
                command_list_count: state.lists.len(),
                depth_resource_count: state.resources.len(),
                descriptor_count: state.descriptors.len(),
                create_command_queue_calls: state.create_command_queue_calls,
                create_command_list_calls: state.create_command_list_calls,
                create_dsv_calls: state.create_dsv_calls,
                resource_barrier_calls: state.resource_barrier_calls,
                om_set_render_targets_calls: state.om_set_render_targets_calls,
                clear_depth_calls: state.clear_depth_calls,
                execute_command_lists_calls: state.execute_command_lists_calls,
                executed_lists: state.executed_lists,
                eligible_pre_clear_snapshots: state.eligible_pre_clear_snapshots,
                descriptor_overwrites: state.descriptor_overwrites,
                present_calls: state.present_calls,
                candidates_dropped: state.candidates_dropped,
                depth_candidates: state.candidate_observations(),
                registered_swapchain_count,
                capture_safe: state.blockers.is_empty(),
                snapshot_copy_implemented: true,
                fence_readback_implemented: true,
                capture_armed: false,
                capture_in_flight: 0,
                capture_dropped_pool_full: 0,
                capture_error: None,
                blocker_counts: state
                    .blocker_counts
                    .iter()
                    .map(|(class, &count)| D3d12BlockerCountObservation {
                        class: class.clone(),
                        count,
                    })
                    .collect(),
                blockers: state.blockers.clone(),
            },
            Err(_) => D3d12ObservationSnapshot {
                resource_epoch: 0,
                queue_count: 0,
                command_list_count: 0,
                depth_resource_count: 0,
                descriptor_count: 0,
                create_command_queue_calls: 0,
                create_command_list_calls: 0,
                create_dsv_calls: 0,
                resource_barrier_calls: 0,
                om_set_render_targets_calls: 0,
                clear_depth_calls: 0,
                execute_command_lists_calls: 0,
                executed_lists: 0,
                eligible_pre_clear_snapshots: 0,
                descriptor_overwrites: 0,
                present_calls: 0,
                candidates_dropped: 0,
                depth_candidates: Vec::new(),
                registered_swapchain_count: 0,
                capture_safe: false,
                snapshot_copy_implemented: true,
                fence_readback_implemented: true,
                capture_armed: false,
                capture_in_flight: 0,
                capture_dropped_pool_full: 0,
                capture_error: Some("D3D12 observation state mutex poisoned".into()),
                blocker_counts: vec![D3d12BlockerCountObservation {
                    class: "observation_state_poisoned".into(),
                    count: 1,
                }],
                blockers: vec!["D3D12 observation state mutex poisoned".into()],
            },
        };
        if self.capture_armed.load(Ordering::Acquire) {
            match self.capture.try_lock() {
                Ok(capture) => {
                    if let Some(capture) = capture.as_ref() {
                        snapshot.capture_armed = true;
                        snapshot.capture_in_flight = capture.in_flight();
                        snapshot.capture_dropped_pool_full = capture.dropped_pool_full();
                        snapshot.capture_error = capture.fatal_error().map(ToOwned::to_owned);
                        if snapshot.capture_error.is_some() {
                            snapshot.capture_safe = false;
                        }
                    } else {
                        snapshot.capture_safe = false;
                        snapshot.capture_error =
                            Some("D3D12 capture armed flag had no capture ring".into());
                    }
                }
                Err(_) => {
                    snapshot.capture_safe = false;
                    snapshot.capture_error = Some("D3D12 capture state is busy/poisoned".into());
                }
            }
        }
        if self.capture_association_fault.load(Ordering::Acquire) {
            snapshot.capture_safe = false;
            snapshot.capture_error =
                Some("D3D12 capture lost a required DSV/list/Execute association".into());
        }
        snapshot
    }

    pub(crate) fn render_camera_present_fields(&self, present_seq: u64) -> (bool, bool, bool) {
        let _timer = self.camera_present_summary_timing.start();
        let lock_timer = self.camera_present_lock_wait_timing.start();
        let tracker = self.render_camera.lock();
        drop(lock_timer);
        let external_loss = self.render_camera_contention_drops.load(Ordering::Acquire) != 0
            || self.hot_lists.drops.load(Ordering::Acquire) != 0;
        tracker.map_or((false, false, false), |tracker| {
            tracker.exact_present_fields(present_seq, external_loss)
        })
    }

    #[must_use]
    pub fn render_camera_snapshot(&self) -> D3d12RenderCameraSnapshot {
        let timer = self.camera_snapshot_timing.start();
        let lock_timer = self.camera_snapshot_lock_wait_timing.start();
        let tracker = self.render_camera.lock();
        drop(lock_timer);
        let mut snapshot = {
            let _copy_timer = self.camera_snapshot_copy_timing.start();
            tracker.map_or_else(
                |_| D3d12RenderCameraSnapshot::default(),
                |tracker| tracker.snapshot_without_admission(),
            )
        };
        snapshot.tracker_contention_drops =
            self.render_camera_contention_drops.load(Ordering::Acquire);
        snapshot.hot_state_drops = self.hot_lists.drops.load(Ordering::Acquire);
        snapshot.root_cbv_calls = self.direct_root_cbv_calls.load(Ordering::Acquire);
        snapshot.descriptor_table_calls = self.descriptor_table_calls.load(Ordering::Acquire);
        {
            let _admission_timer = self.camera_snapshot_admission_timing.start();
            snapshot.refresh_authoring_admission();
        }
        drop(timer);
        snapshot.hook_cpu_timing_scope = "probe_lifetime".into();
        // Preserve the legacy field name while explicitly stating what Instant measures.
        snapshot.hook_timing_measurement =
            "wall_elapsed_including_lock_wait; concurrent/nested_stages_not_additive; not_thread_cpu_or_capture_window".into();
        snapshot.original_target_cache_entries = self.original_targets.len();
        snapshot.hook_cpu_timing = self.hook_timing_snapshot();
        snapshot
    }

    /// Cold checkpoint: no camera history clone or admission work. Descriptor update
    /// counts visit one shard at a time; this is not a lock-free hot-path operation.
    pub(crate) fn hook_timing_snapshot(
        &self,
    ) -> BTreeMap<String, crate::hook_timing::TimingSnapshot> {
        let batch_bucket = |index: usize| crate::hook_timing::TimingSnapshot {
            calls: self.descriptor_copy_batch_buckets[index].load(Ordering::Relaxed),
            ..Default::default()
        };
        let shard_bucket = |index: usize| crate::hook_timing::TimingSnapshot {
            calls: self.descriptor_copy_shard_buckets[index].load(Ordering::Relaxed),
            ..Default::default()
        };
        let count = |calls| crate::hook_timing::TimingSnapshot {
            calls,
            ..Default::default()
        };
        let production_draw = self.hot_lists.production_draw_metrics();
        let mut timings = BTreeMap::from([
            (
                "execute_prepare".into(),
                self.camera_execute_prepare_timing.snapshot(),
            ),
            (
                "execute_complete".into(),
                self.camera_execute_complete_timing.snapshot(),
            ),
            (
                "selected_draw".into(),
                self.camera_selected_draw_timing.snapshot(),
            ),
            (
                "selected_draw_lock_wait".into(),
                self.camera_selected_draw_lock_wait_timing.snapshot(),
            ),
            (
                "selected_draw_locked".into(),
                self.camera_selected_draw_locked_timing.snapshot(),
            ),
            (
                "camera_scissor_observation".into(),
                self.camera_scissor_timing.snapshot(),
            ),
            (
                "original_target_cache_miss".into(),
                self.original_target_miss_timing.snapshot(),
            ),
            (
                "descriptor_write".into(),
                self.descriptor_write_timing.snapshot(),
            ),
            (
                "descriptor_invalidate".into(),
                self.descriptor_invalidate_timing.snapshot(),
            ),
            (
                "descriptor_copy".into(),
                self.descriptor_copy_timing.snapshot(),
            ),
            (
                // A count, not a duration: the timing fields stay zero.
                "descriptor_copy_untracked".into(),
                crate::hook_timing::TimingSnapshot {
                    calls: self.descriptor_copy_untracked.load(Ordering::Relaxed),
                    ..Default::default()
                },
            ),
            (
                // Also a count: descriptors moved, not calls made.
                "descriptor_copy_descriptors".into(),
                crate::hook_timing::TimingSnapshot {
                    calls: self.descriptor_copy_descriptors.load(Ordering::Relaxed),
                    ..Default::default()
                },
            ),
            (
                // Counts only: batch-size histogram buckets 1 / 2-8 / 9-32 /
                // 33-128 / 129-512 / 513-4096.
                "descriptor_copy_batch_1".into(),
                batch_bucket(0),
            ),
            ("descriptor_copy_batch_2_8".into(), batch_bucket(1)),
            ("descriptor_copy_batch_9_32".into(), batch_bucket(2)),
            ("descriptor_copy_batch_33_128".into(), batch_bucket(3)),
            ("descriptor_copy_batch_129_512".into(), batch_bucket(4)),
            ("descriptor_copy_batch_513_4096".into(), batch_bucket(5)),
            ("descriptor_copy_shards_1".into(), shard_bucket(0)),
            ("descriptor_copy_shards_2".into(), shard_bucket(1)),
            ("descriptor_copy_shards_3_4".into(), shard_bucket(2)),
            ("descriptor_copy_shards_5_8".into(), shard_bucket(3)),
            ("descriptor_copy_shards_9_plus".into(), shard_bucket(4)),
            (
                "descriptor_copy_prepare".into(),
                self.descriptor_copy_prepare_timing.snapshot(),
            ),
            (
                "descriptor_copy_lock_wait".into(),
                self.descriptor_copy_lock_wait_timing.snapshot(),
            ),
            (
                "descriptor_copy_apply".into(),
                self.descriptor_copy_apply_timing.snapshot(),
            ),
            (
                // A count: how often the released-heap scan is actually paid.
                "descriptor_heap_releases".into(),
                crate::hook_timing::TimingSnapshot {
                    calls: self.descriptor_heap_releases.load(Ordering::Relaxed),
                    ..Default::default()
                },
            ),
            ("resource_map".into(), self.resource_map_timing.snapshot()),
            (
                "camera_discovery_draw_sampled".into(),
                crate::hook_timing::TimingSnapshot {
                    calls: self.camera_discovery_draw_sampled.load(Ordering::Relaxed),
                    ..Default::default()
                },
            ),
            (
                "camera_discovery_list_budget_skipped".into(),
                crate::hook_timing::TimingSnapshot {
                    calls: self
                        .camera_discovery_list_budget_skipped
                        .load(Ordering::Relaxed),
                    ..Default::default()
                },
            ),
            (
                "resource_unmap".into(),
                self.resource_unmap_timing.snapshot(),
            ),
            (
                "buffer_copy_provenance".into(),
                self.buffer_copy_timing.snapshot(),
            ),
            (
                "observation_snapshot".into(),
                self.observation_snapshot_timing.snapshot(),
            ),
            (
                "camera_snapshot".into(),
                self.camera_snapshot_timing.snapshot(),
            ),
            (
                "camera_snapshot_lock_wait".into(),
                self.camera_snapshot_lock_wait_timing.snapshot(),
            ),
            (
                "camera_snapshot_copy".into(),
                self.camera_snapshot_copy_timing.snapshot(),
            ),
            (
                "camera_snapshot_admission".into(),
                self.camera_snapshot_admission_timing.snapshot(),
            ),
            (
                "camera_present_summary".into(),
                self.camera_present_summary_timing.snapshot(),
            ),
            (
                "camera_present_lock_wait".into(),
                self.camera_present_lock_wait_timing.snapshot(),
            ),
        ]);
        self.camera_execute_prepare_details
            .append_snapshot(&mut timings);
        if let Some((same, reindexed)) = self.cbv_descriptor_store.update_counts() {
            timings.insert("descriptor_update_same_gpuva".into(), count(same));
            timings.insert("descriptor_update_reindexed".into(), count(reindexed));
        }
        for (name, calls) in [
            ("production_draw_prefilter_admitted", production_draw[0]),
            ("production_draw_prefilter_rejected", production_draw[1]),
            ("production_draw_lock_immediate", production_draw[2]),
            ("production_draw_lock_contended", production_draw[3]),
            ("production_draw_post_lock_rejected", production_draw[4]),
            ("production_draw_budget_rejected", production_draw[5]),
            ("production_draw_tracker_called", production_draw[6]),
            ("production_draw_redundant", production_draw[7]),
            (
                "execute_prepare_lock_immediate",
                self.camera_execute_prepare_lock_immediate
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_prepare_lock_contended",
                self.camera_execute_prepare_lock_contended
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_complete_lock_immediate",
                self.camera_execute_complete_lock_immediate
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_complete_lock_contended",
                self.camera_execute_complete_lock_contended
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_with_prepared_copy_shadows",
                self.camera_execute_with_prepared_copy_shadows
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_prepared_copy_shadows",
                self.camera_execute_prepared_copy_shadows
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_with_depth_copy_proof",
                self.camera_execute_with_depth_copy_proof
                    .load(Ordering::Relaxed),
            ),
            (
                "execute_with_camera_work",
                self.camera_execute_with_camera_work.load(Ordering::Relaxed),
            ),
            (
                "execute_skipped_no_camera_work",
                self.camera_execute_skipped_no_camera_work
                    .load(Ordering::Relaxed),
            ),
            (
                "production_camera_take_lock_immediate",
                self.production_camera_take_lock_immediate
                    .load(Ordering::Relaxed),
            ),
            (
                "production_camera_take_lock_contended",
                self.production_camera_take_lock_contended
                    .load(Ordering::Relaxed),
            ),
        ] {
            timings.insert(name.into(), count(calls));
        }
        self.pso_registry.append_counters(&mut timings);
        timings
    }

    pub(crate) fn local_camera_samples(
        &self,
        present: u64,
        width: u32,
        height: u32,
    ) -> Result<crate::d3d12_render_camera::local_admission::LocalSamples, String> {
        self.selected_capture_health()?;
        let tracker_drops = self.render_camera_contention_drops.load(Ordering::Acquire);
        let hot_state_drops = self.hot_lists.drops.load(Ordering::Acquire);
        if tracker_drops != 0 || hot_state_drops != 0 {
            return Err(format!(
                "local camera observation coverage is unsafe: tracker_contention_drops={tracker_drops}, hot_state_drops={hot_state_drops}"
            ));
        }
        self.render_camera
            .lock()
            .map_err(|_| "local camera tracker poisoned")?
            .local_camera_samples(present, width, height)
    }

    pub(crate) fn archive_local_camera_before(
        &self,
        present: u64,
        enqueue: impl FnOnce(
            Vec<crate::d3d12_render_camera::D3d12RenderDrawObservation>,
        )
            -> Result<(), Vec<crate::d3d12_render_camera::D3d12RenderDrawObservation>>,
    ) -> Result<(), String> {
        self.render_camera
            .lock()
            .map_err(|_| "camera archive tracker poisoned")?
            .archive_local_before(present, enqueue)
    }

    pub(crate) fn freeze_local_camera_archive(&self) -> Result<(), String> {
        // Stop new diagnostic sampling before sealing pose/report data. Current records stay.
        if crate::raw_depth_demo::camera_archive_enabled() {
            self.disarm_selected_depth_hot_path();
            self.render_camera
                .lock()
                .map_err(|_| "camera archive tracker poisoned")?
                .freeze_local_observation_archive();
        }
        Ok(())
    }

    pub fn configure_production_camera(
        &self,
        binding: depth_lab::D3d12CameraBinding,
        context: depth_lab::CameraPoseStreamContext,
    ) -> Result<(), String> {
        let holstered = binding.holstered_camera.is_some();
        let draw_filter = ProductionDrawFilter::from_binding(&binding);
        let mut tracker = self
            .render_camera
            .lock()
            .map_err(|_| "D3D12 camera tracker poisoned")?;
        if self
            .production_draw_filter
            .get()
            .is_some_and(|existing| existing != &draw_filter)
        {
            return Err("D3D12 production draw filter changed within one process".into());
        }
        tracker.configure_production(
            binding,
            context,
            (
                self.render_camera_contention_drops.load(Ordering::Acquire),
                self.hot_lists.drops.load(Ordering::Acquire),
            ),
        )?;
        if self.production_draw_filter.get().is_none() {
            self.production_draw_filter
                .set(draw_filter)
                .map_err(|_| "D3D12 production draw filter publication raced")?;
        }
        // Publish the new mode while the tracker is still exclusively configured. A Draw that
        // observes either flag afterwards blocks on this guard and can only see the new tracker.
        // Lists belonging to Draws that observed the old mode are conservatively marked below;
        // this closes the activation race for Execute's no-camera-work fast path.
        self.render_camera_draw_budget
            .store(4096, Ordering::Release);
        self.bound_holstered_camera
            .store(holstered, Ordering::Release);
        self.render_camera_production.store(true, Ordering::Release);
        if holstered {
            self.hot_lists.mark_registered_production_camera_work();
        }
        drop(tracker);
        Ok(())
    }

    fn draw_pass_evidence_enabled(&self) -> bool {
        self.depth_order_diagnostics.load(Ordering::Acquire)
            || self.bound_holstered_camera.load(Ordering::Acquire)
    }

    fn production_draw_filter_match(&self, list_raw: usize) -> Option<(&HotListSlot, bool)> {
        let Some(filter) = self.production_draw_filter.get() else {
            return self.hot_lists.find(list_raw).map(|slot| (slot, false));
        };
        let Some(root_signature) = self.hot_lists.root_signature(list_raw) else {
            return self.hot_lists.find(list_raw).map(|slot| (slot, false));
        };
        // Scope the independent metadata guard before the render-camera mutex. Root-signature
        // Release takes those cold-path guards in the opposite order.
        let Ok(fingerprints) = self.root_fingerprints.read() else {
            return self.hot_lists.find(list_raw).map(|slot| (slot, false));
        };
        let Some(actual) = fingerprints.get(&root_signature) else {
            return self.hot_lists.find(list_raw).map(|slot| (slot, false));
        };
        self.hot_lists
            .production_draw_filter_match(list_raw, actual, filter)
    }

    #[cfg(test)]
    fn matches_production_draw_filter(&self, list_raw: usize) -> bool {
        self.production_draw_filter_match(list_raw)
            .is_some_and(|(_, matched)| matched)
    }

    pub(crate) fn selected_interval_capture_requested(&self, accepting_candidates: bool) -> bool {
        // Bound CP2077 camera admission needs the current copy's actual GPU
        // ordering even while the client awaits Ready/Arm or has sent Seal.
        // Outside acceptance the selected drain releases these proof snapshots;
        // they never enter the candidate ring, readback budget or depth writer.
        accepting_candidates
            || (self.render_camera_production.load(Ordering::Acquire)
                && self.bound_holstered_camera.load(Ordering::Acquire))
    }

    pub(crate) fn take_production_camera(
        &self,
        present_seq: u64,
    ) -> Result<Option<crate::d3d12_render_camera::ProductionCameraSample>, String> {
        let tracker = if self.bound_holstered_camera.load(Ordering::Acquire) {
            // Consume the same CPU-side ledger that before_present just sealed. Ordinary
            // bookkeeping contention must not discard a bound CP2077 camera interval.
            self.lock_render_camera_measured(
                self.production_performance_metrics_enabled(),
                &self.production_camera_take_lock_immediate,
                &self.production_camera_take_lock_contended,
            )
            .map_err(std::sync::TryLockError::Poisoned)
        } else {
            self.render_camera.try_lock()
        };
        tracker
            .map_err(|error| match error {
                std::sync::TryLockError::WouldBlock => "D3D12 camera tracker contended at Present",
                std::sync::TryLockError::Poisoned(_) => "D3D12 camera tracker poisoned at Present",
            })?
            .take_production_camera_sample(
                present_seq,
                (
                    self.render_camera_contention_drops.load(Ordering::Acquire),
                    self.hot_lists.drops.load(Ordering::Acquire),
                ),
            )
    }

    fn arm_selected_depth_hot_path(&self, identity: DsvIdentity) {
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        if crate::raw_depth_demo::active()
            && let Ok(mut tracker) = self.render_camera.lock()
        {
            tracker.enable_depth_order_diagnostics();
            if crate::raw_depth_demo::camera_archive_enabled() {
                tracker.enable_local_observation_archive();
            }
            self.depth_order_diagnostics.store(true, Ordering::Release);
        }
        self.publish_selected_depth_hot_path(identity);
    }

    // Phase transitions already hold the tracker lock and must not initialize/reset archives.
    fn publish_selected_depth_hot_path(&self, identity: DsvIdentity) {
        self.selected_depth_token.store(0, Ordering::Release);
        self.selected_depth_handle
            .store(identity.handle.0, Ordering::Relaxed);
        self.selected_depth_descriptor_generation
            .store(identity.descriptor_generation, Ordering::Relaxed);
        self.selected_depth_resource
            .store(identity.resource.0, Ordering::Relaxed);
        self.selected_depth_resource_epoch
            .store(identity.resource_epoch, Ordering::Relaxed);
        let token = self
            .selected_depth_next_token
            .fetch_add(1, Ordering::Relaxed)
            .max(1);
        self.selected_depth_token.store(token, Ordering::Release);
    }

    fn disarm_selected_depth_hot_path(&self) {
        self.bound_holstered_camera.store(false, Ordering::Release);
        self.depth_order_diagnostics.store(false, Ordering::Release);
        self.selected_depth_token.store(0, Ordering::Release);
    }

    fn selected_depth_token_for(&self, identity: Option<DsvIdentity>) -> u64 {
        let Some(identity) = identity else {
            return 0;
        };
        let token = self.selected_depth_token.load(Ordering::Acquire);
        if token == 0
            || self.selected_depth_handle.load(Ordering::Relaxed) != identity.handle.0
            || self
                .selected_depth_descriptor_generation
                .load(Ordering::Relaxed)
                != identity.descriptor_generation
            || self.selected_depth_resource.load(Ordering::Relaxed) != identity.resource.0
            || self.selected_depth_resource_epoch.load(Ordering::Relaxed) != identity.resource_epoch
            || self.selected_depth_token.load(Ordering::Acquire) != token
        {
            0
        } else {
            token
        }
    }

    /// Arms one exact descriptor/resource identity after an observation run has
    /// proven it is the unique scene DSV. This never selects a DSV heuristically.
    pub fn arm_depth_capture_by_handle(
        &self,
        descriptor_handle: u64,
        slot_count: usize,
        exact_transition: Option<depth_lab::D3d12TransitionIdentity>,
        fallback_draw: Option<depth_lab::D3d12FallbackDrawIdentity>,
    ) -> Result<DsvIdentity, String> {
        self.arm_depth_capture_storage(
            descriptor_handle,
            slot_count,
            false,
            exact_transition,
            fallback_draw,
        )
    }

    fn arm_depth_capture_storage(
        &self,
        descriptor_handle: u64,
        slot_count: usize,
        gpu_only: bool,
        exact_transition: Option<depth_lab::D3d12TransitionIdentity>,
        fallback_draw: Option<depth_lab::D3d12FallbackDrawIdentity>,
    ) -> Result<DsvIdentity, String> {
        let (identity, resource_raw, dsv_format) = {
            let state = self
                .state
                .lock()
                .map_err(|_| "D3D12 observation state mutex poisoned".to_string())?;
            let identity = state
                .resolve_handle(descriptor_handle)
                .ok_or_else(|| "cannot arm an unknown/null DSV descriptor".to_string())?;
            let resource_raw = state
                .resources
                .iter()
                .find_map(|(raw, resource)| (resource.id == identity.resource).then_some(*raw))
                .ok_or_else(|| "selected DSV has no live resource".to_string())?;
            let dsv_format = state
                .candidates
                .values()
                .find(|candidate| candidate.identity == identity)
                .map(|candidate| candidate.metadata.dsv_format)
                .ok_or_else(|| "selected DSV has no exact discovery metadata".to_string())?;
            (identity, resource_raw, dsv_format)
        };
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned".to_string())?;
        if capture.is_some() {
            return Err("D3D12 depth capture is already armed".into());
        }
        self.capture_association_fault
            .store(false, Ordering::Release);
        let resource_ptr = resource_raw as *mut c_void;
        let source = unsafe { ID3D12Resource::from_raw_borrowed(&resource_ptr) }
            .ok_or("selected DSV resource pointer is invalid")?;
        let mut device = None;
        unsafe { source.GetDevice(&mut device) }
            .map_err(|error| format!("selected DSV GetDevice failed: {error}"))?;
        let device: ID3D12Device = device.ok_or("selected DSV GetDevice returned no device")?;
        *capture = Some(D3d12CaptureRing::new_with_storage(
            &device,
            source,
            identity,
            DXGI_FORMAT(dsv_format),
            slot_count,
            gpu_only,
        )?);
        self.render_camera
            .lock()
            .map_err(|_| "D3D12 render-camera tracker mutex poisoned".to_string())?
            .arm_selected_depth(identity);
        self.diagnostic_depth_resource_raw
            .store(resource_raw, Ordering::Relaxed);
        self.arm_selected_depth_hot_path(identity);
        if let Some(transition) = exact_transition {
            self.capture_transition_before
                .store(transition.state_before, Ordering::Relaxed);
            self.capture_transition_after
                .store(transition.state_after, Ordering::Relaxed);
            self.capture_transition_subresource
                .store(transition.subresource, Ordering::Relaxed);
            self.capture_transition_flags
                .store(transition.barrier_flags, Ordering::Relaxed);
            self.capture_exact_transition_enabled
                .store(true, Ordering::Release);
        } else {
            self.capture_exact_transition_enabled
                .store(false, Ordering::Release);
        }
        *self
            .capture_fallback_draw
            .lock()
            .map_err(|_| "D3D12 fallback draw lock poisoned".to_string())? = fallback_draw;
        self.capture_armed.store(true, Ordering::Release);
        Ok(identity)
    }

    /// Arms only a single current candidate matching the operator-pinned
    /// discovery identity and exact geometry/format. No ranking or fallback is
    /// permitted, and at least two clear-to-Present intervals must be observed.
    pub fn arm_unique_depth_capture(
        &self,
        selector: &D3d12DepthSelector,
        slot_count: usize,
    ) -> Result<DsvIdentity, String> {
        self.arm_unique_depth_capture_storage(selector, slot_count, false)
    }

    pub fn arm_unique_depth_capture_storage(
        &self,
        selector: &D3d12DepthSelector,
        slot_count: usize,
        gpu_only: bool,
    ) -> Result<DsvIdentity, String> {
        let (handle, exact_transition, fallback_draw) = {
            let state = self
                .state
                .lock()
                .map_err(|_| "D3D12 observation state mutex poisoned".to_string())?;
            let matching: Vec<_> = state
                .candidates
                .values()
                .filter(|candidate| {
                    let id = format!(
                        "dsv-r{}-g{}-e{}",
                        candidate.identity.resource.0,
                        candidate.identity.descriptor_generation,
                        candidate.identity.resource_epoch
                    );
                    let identity_matches = selector.stable_selection.as_ref().map_or_else(
                        || id == selector.candidate_id,
                        |selection| stable_candidate_matches(candidate, selection),
                    );
                    identity_matches
                        && candidate.metadata.width == u64::from(selector.width)
                        && candidate.metadata.height == selector.height
                        && candidate.metadata.sample_count == selector.sample_count
                        && selector
                            .accepted_formats
                            .contains(&candidate.metadata.resource_format)
                        && selector
                            .accepted_formats
                            .contains(&candidate.metadata.dsv_format)
                        && candidate.eligible_clear_calls >= 2
                        && candidate.present_intervals_with_clear >= 2
                        && state.identity_is_current(candidate.identity)
                })
                .collect();
            if matching.len() != 1 {
                return Err(format!(
                    "exact D3D12 profile matched {} proven current candidates",
                    matching.len()
                ));
            }
            (
                matching[0].identity.handle.0,
                selector
                    .stable_selection
                    .as_ref()
                    .map(|selection| selection.transition),
                selector
                    .stable_selection
                    .as_ref()
                    .and_then(|selection| selection.fallback_draw.clone()),
            )
        };
        self.arm_depth_capture_storage(
            handle,
            slot_count,
            gpu_only,
            exact_transition,
            fallback_draw,
        )
    }

    pub fn take_gpu_depth_snapshots(
        &self,
    ) -> Result<Vec<crate::d3d12_capture::GpuDepthSnapshot>, String> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned")?;
        if self.capture_association_fault.swap(false, Ordering::AcqRel) {
            let failed = capture.take();
            self.capture_armed.store(false, Ordering::Release);
            drop(failed);
            return Err("D3D12 GPU snapshot lost its Execute association".into());
        }
        capture
            .as_mut()
            .ok_or("D3D12 GPU capture is not armed")?
            .take_gpu_snapshots()
    }

    pub fn gpu_snapshot_storage_bytes(&self) -> Result<u64, String> {
        self.capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned")?
            .as_ref()
            .map(D3d12CaptureRing::gpu_storage_bytes)
            .ok_or_else(|| "D3D12 GPU capture is not armed".into())
    }

    pub fn release_gpu_depth_snapshot(
        &self,
        snapshot: &crate::d3d12_capture::GpuDepthSnapshot,
        consumer: Option<(windows::Win32::Graphics::Direct3D12::ID3D12Fence, u64)>,
    ) -> Result<(), String> {
        self.capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned")?
            .as_mut()
            .ok_or("D3D12 GPU capture is not armed")?
            .release_gpu_snapshot(snapshot, consumer)
    }

    pub fn drain_depth_capture(&self) -> Result<Vec<D3d12RawDepthFrame>, String> {
        let mut capture = self
            .capture
            .try_lock()
            .map_err(|_| "D3D12 capture drain would block".to_string())?;
        if self.capture_association_fault.swap(false, Ordering::AcqRel) {
            // Execute already ran without a fence association. Dropping the
            // non-idle ring intentionally leaks its extra GPU-visible COM refs
            // (see D3d12CaptureRing::Drop), then makes StopRun finite.
            let failed = capture.take();
            self.capture_armed.store(false, Ordering::Release);
            drop(failed);
            return Err(
                "D3D12 capture lost a required DSV/list/Execute association and was retired".into(),
            );
        }
        capture
            .as_mut()
            .ok_or_else(|| "D3D12 depth capture is not armed".to_string())?
            .drain_ready()
    }

    pub fn disarm_depth_capture(&self) -> Result<(), String> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned".to_string())?;
        if capture.as_ref().is_some_and(|ring| !ring.is_idle()) {
            return Err("cannot disarm D3D12 capture with in-flight slots".into());
        }
        *capture = None;
        self.render_camera
            .lock()
            .map_err(|_| "D3D12 render-camera tracker mutex poisoned".to_string())?
            .disarm();
        self.render_camera_production
            .store(false, Ordering::Release);
        self.disarm_selected_depth_hot_path();
        self.capture_exact_transition_enabled
            .store(false, Ordering::Release);
        if let Ok(mut fallback) = self.capture_fallback_draw.lock() {
            *fallback = None;
        }
        self.capture_armed.store(false, Ordering::Release);
        self.capture_association_fault
            .store(false, Ordering::Release);
        Ok(())
    }

    pub(crate) fn authoring_boundary_is_drained(&self) -> Result<bool, String> {
        if !self.capture_is_armed() {
            return Ok(true);
        }
        if self.capture_association_fault.load(Ordering::Acquire) {
            return Err("authoring cannot recover a failed Execute association".into());
        }
        let capture = self
            .capture
            .lock()
            .map_err(|_| "authoring capture poisoned")?;
        let ring = capture.as_ref().ok_or("authoring capture disappeared")?;
        if let Some(error) = ring.fatal_error() {
            return Err(error.into());
        }
        Ok(ring.authoring_boundary_is_drained()
            && self
                .render_camera
                .lock()
                .map_err(|_| "authoring tracker poisoned")?
                .authoring_boundary_is_drained())
    }

    pub(crate) fn rebind_authoring_source(
        &self,
        selector: &D3d12DepthSelector,
    ) -> Result<DsvIdentity, String> {
        if !cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
            || !crate::raw_depth_demo::authoring_windows_enabled()
            || crate::raw_depth_demo::authoring_camera_interval_active()
            || self.render_camera_production.load(Ordering::Acquire)
            || self.capture_association_fault.load(Ordering::Acquire)
        {
            return Err(
                "source change is only allowed at a healthy diagnostic authoring boundary".into(),
            );
        }
        let (identity, source, dsv_format) = {
            let state = self
                .state
                .lock()
                .map_err(|_| "authoring observation poisoned")?;
            let candidate = state
                .candidates
                .values()
                .find(|c| {
                    format!(
                        "dsv-r{}-g{}-e{}",
                        c.identity.resource.0,
                        c.identity.descriptor_generation,
                        c.identity.resource_epoch
                    ) == selector.candidate_id
                })
                .ok_or("authoring source disappeared")?;
            if !state.identity_is_current(candidate.identity)
                || candidate.metadata.width != u64::from(selector.width)
                || candidate.metadata.height != selector.height
                || candidate.metadata.sample_count != selector.sample_count
                || !selector
                    .accepted_formats
                    .contains(&candidate.metadata.resource_format)
                || !selector
                    .accepted_formats
                    .contains(&candidate.metadata.dsv_format)
            {
                return Err("authoring selected source identity/geometry drifted".into());
            }
            let raw = state
                .resources
                .iter()
                .find_map(|(raw, r)| (r.id == candidate.identity.resource).then_some(*raw))
                .ok_or("authoring source has no live resource")?
                as *mut c_void;
            let source = unsafe { ID3D12Resource::from_raw_borrowed(&raw) }
                .ok_or("authoring resource is null")?
                .clone();
            (candidate.identity, source, candidate.metadata.dsv_format)
        };
        // Same capture -> tracker order as Execute; holding both prevents clearing or
        // reinterpreting late work. A delayed draw rechecks its token under the tracker lock.
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| "authoring capture poisoned")?;
        let ring = capture.as_mut().ok_or("authoring ring missing")?;
        let mut tracker = self
            .render_camera
            .lock()
            .map_err(|_| "authoring tracker poisoned")?;
        if !tracker.authoring_boundary_is_drained() {
            return Err("authoring camera did not drain".into());
        }
        let retired = ring.rebind_authoring_source(&source, identity, DXGI_FORMAT(dsv_format))?;
        let rebound = tracker.rebind_authoring_source(identity);
        if rebound.is_ok() {
            self.diagnostic_depth_resource_raw
                .store(source.as_raw() as usize, Ordering::Relaxed);
            self.publish_selected_depth_hot_path(identity);
        }
        drop(tracker);
        drop(capture);
        // The old texture may now reach Release(0), re-entering state/tracker hooks.
        drop(retired);
        rebound?;
        Ok(identity)
    }

    /// Fault retirement keeps in-flight COM resources alive via CaptureRing::Drop.
    /// It clears the run's ownership so the control lifecycle cannot hang forever.
    pub fn abort_depth_capture(&self) -> Result<(), String> {
        self.capture_armed.store(false, Ordering::Release);
        let capture = self
            .capture
            .lock()
            .map_err(|_| "D3D12 capture mutex poisoned")?
            .take();
        drop(capture);
        self.render_camera
            .lock()
            .map_err(|_| "D3D12 render-camera tracker poisoned")?
            .disarm();
        self.render_camera_production
            .store(false, Ordering::Release);
        self.disarm_selected_depth_hot_path();
        self.capture_association_fault
            .store(false, Ordering::Release);
        Ok(())
    }

    unsafe fn install_queue(&self, queue: *mut c_void) -> Result<(), String> {
        unsafe {
            self.install_method(
                queue,
                iunknown_release_index(),
                HookMethod::ReleaseQueue,
                hook_release_queue as *const () as usize,
            )?;
            self.install_method(
                queue,
                d3d12_abi::command_queue::EXECUTE_COMMAND_LISTS,
                HookMethod::ExecuteCommandLists,
                hook_execute_command_lists as *const () as usize,
            )
        }
    }

    unsafe fn install_list(&self, list: *mut c_void) -> Result<(), String> {
        unsafe {
            self.install_method(
                list,
                iunknown_release_index(),
                HookMethod::ReleaseList,
                hook_release_list as *const () as usize,
            )?;
            // PSO instrumentation is optional observation, never a capture/admission gate.
            // Failed coverage must also hide the Reset/Create initial binding: later Set calls
            // would otherwise silently leave that value stale.
            let pipeline_hook_available = self
                .install_method(
                    list,
                    pipeline_depth::SET_PIPELINE,
                    HookMethod::SetPipelineState,
                    pipeline_depth::hook_set_pipeline_state as *const () as usize,
                )
                .is_ok();
            self.hot_lists
                .set_pipeline_hook_available(list as usize, pipeline_hook_available);
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::RESET,
                HookMethod::Reset,
                hook_reset as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::RS_SET_VIEWPORTS,
                HookMethod::RsSetViewports,
                hook_rs_set_viewports as *const () as usize,
            )?;
            if cfg!(all(feature = "cp2077-diagnostics", debug_assertions)) {
                // Optional diagnosis: absent coverage remains unknown, never a capture gate.
                let _ = self.install_method(
                    list,
                    d3d12_abi::graphics_command_list::RS_SET_SCISSOR_RECTS,
                    HookMethod::RsSetScissorRects,
                    hook_rs_set_scissor_rects as *const () as usize,
                );
            }
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::RESOURCE_BARRIER,
                HookMethod::ResourceBarrier,
                hook_resource_barrier as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::COPY_BUFFER_REGION,
                HookMethod::CopyBufferRegion,
                hook_copy_buffer_region as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::OM_SET_RENDER_TARGETS,
                HookMethod::OmSetRenderTargets,
                hook_om_set_render_targets as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::CLEAR_DEPTH_STENCIL_VIEW,
                HookMethod::ClearDepthStencilView,
                hook_clear_depth_stencil_view as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::SET_GRAPHICS_ROOT_CONSTANT_BUFFER_VIEW,
                HookMethod::SetGraphicsRootConstantBufferView,
                hook_set_graphics_root_constant_buffer_view as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::SET_DESCRIPTOR_HEAPS,
                HookMethod::SetDescriptorHeaps,
                hook_set_descriptor_heaps as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::SET_GRAPHICS_ROOT_DESCRIPTOR_TABLE,
                HookMethod::SetGraphicsRootDescriptorTable,
                hook_set_graphics_root_descriptor_table as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::SET_GRAPHICS_ROOT_SIGNATURE,
                HookMethod::SetGraphicsRootSignature,
                hook_set_graphics_root_signature as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::DRAW_INSTANCED,
                HookMethod::DrawInstanced,
                hook_draw_instanced as *const () as usize,
            )?;
            self.install_method(
                list,
                d3d12_abi::graphics_command_list::DRAW_INDEXED_INSTANCED,
                HookMethod::DrawIndexedInstanced,
                hook_draw_indexed_instanced as *const () as usize,
            )
        }
    }

    unsafe fn install_resource(&self, resource: *mut c_void) -> Result<(), String> {
        unsafe {
            self.install_method(
                resource,
                iunknown_release_index(),
                HookMethod::ReleaseResource,
                hook_release_resource as *const () as usize,
            )?;
            self.install_method(
                resource,
                d3d12_abi::resource::MAP,
                HookMethod::MapResource,
                hook_map_resource as *const () as usize,
            )?;
            self.install_method(
                resource,
                d3d12_abi::resource::UNMAP,
                HookMethod::UnmapResource,
                hook_unmap_resource as *const () as usize,
            )
        }
    }

    unsafe fn install_descriptor_heap(&self, heap: *mut c_void) -> Result<(), String> {
        unsafe {
            self.install_method(
                heap,
                iunknown_release_index(),
                HookMethod::ReleaseDescriptorHeap,
                hook_release_descriptor_heap as *const () as usize,
            )
        }
    }

    unsafe fn install_root_signature(&self, root_signature: *mut c_void) -> Result<(), String> {
        unsafe {
            self.install_method(
                root_signature,
                iunknown_release_index(),
                HookMethod::ReleaseRootSignature,
                hook_release_root_signature as *const () as usize,
            )
        }
    }

    unsafe fn install_method(
        &self,
        object: *mut c_void,
        index: usize,
        method: HookMethod,
        replacement: usize,
    ) -> Result<(), String> {
        if object.is_null() {
            return Err(format!("cannot install {method:?} on a null object"));
        }
        let vtable = unsafe { *object.cast::<*mut usize>() };
        if vtable.is_null() {
            return Err(format!("cannot install {method:?} on a null vtable"));
        }
        let slot = unsafe { vtable.add(index) } as usize;
        let mut hooks = self
            .hooks
            .lock()
            .map_err(|_| "D3D12 hook registry mutex poisoned".to_string())?;
        let key = HookKey { slot, method };
        if let Some(original) = hooks.originals.get(&key).copied() {
            let current = unsafe { *(slot as *const usize) };
            return if current == replacement && original != replacement {
                Ok(())
            } else {
                Err(format!(
                    "refusing ambiguous {method:?} hook chain at an already-hooked runtime slot"
                ))
            };
        }
        let current = unsafe { *(slot as *const usize) };
        if current == 0 || current == replacement {
            return Err(format!(
                "refusing {method:?} hook with an invalid/unknown original target"
            ));
        }
        // The registry lock spans replacement + original publication. A hook
        // racing this installation blocks in `original_target` until published.
        let guard = unsafe { VtableHook::install(object, index, replacement) }
            .map_err(|error| format!("installing {method:?} failed: {error}"))?;
        let original = guard.original();
        hooks.originals.insert(key, original);
        hooks.guards.push(guard);
        // Publish one immutable (exact slot + method, original) tuple only after successful
        // installation and ownership of its guard. Unpublished/saturated keys still take hooks.
        self.original_targets.publish(key, original);
        Ok(())
    }

    fn original_target(
        &self,
        object: *mut c_void,
        index: usize,
        method: HookMethod,
    ) -> Option<usize> {
        if object.is_null() {
            return None;
        }
        let vtable = unsafe { *object.cast::<*mut usize>() };
        if vtable.is_null() {
            return None;
        }
        let slot = unsafe { vtable.add(index) } as usize;
        let key = HookKey { slot, method };
        if self.hooks.is_poisoned() {
            return None;
        }
        if let Some(target) = self.original_targets.get(key) {
            return Some(target);
        }
        // No timer or shared counter write on cache hits: these callbacks occur millions of
        // times. A miss still serializes with installation, including original publication.
        let _timer = self.original_target_miss_timing.start();
        self.hooks.lock().ok()?.originals.get(&key).copied()
    }

    fn block(&self, message: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.block(message);
        }
    }

    fn discard_recorded_capture_list(&self, list: usize, operation: &str) {
        if !self.capture_armed.load(Ordering::Acquire) {
            return;
        }
        self.discard_recorded_capture_list_after_admission(list, operation);
    }

    fn discard_recorded_capture_list_after_admission(&self, list: usize, operation: &str) {
        match self.capture.lock() {
            Ok(mut capture) => {
                // Stop/abort can retire the ring while this callback waits for the lock.
                // Admission outside the lock is only a fast path, not proof of live ownership.
                if !self.capture_armed.load(Ordering::Acquire) {
                    return;
                }
                if let Some(capture) = capture.as_mut() {
                    capture.discard_recorded_list(list);
                } else {
                    drop(capture);
                    self.capture_association_fault
                        .store(true, Ordering::Release);
                    self.block(format!(
                        "D3D12 capture was armed without a ring during command-list {operation}"
                    ));
                }
            }
            Err(poisoned) => {
                drop(poisoned.into_inner());
                self.capture_association_fault
                    .store(true, Ordering::Release);
                self.block(format!(
                    "D3D12 capture mutex poisoned during command-list {operation}"
                ));
            }
        }
    }

    fn update_dsv_capture_after_admission(&self, handle: u64, new_identity: Option<DsvIdentity>) {
        match self.capture.lock() {
            Ok(mut capture) => {
                if !self.capture_armed.load(Ordering::Acquire) {
                    return;
                }
                if let Some(capture) = capture.as_mut() {
                    if capture.source_identity().handle.0 == handle
                        && new_identity != Some(capture.source_identity())
                    {
                        capture.invalidate("armed DSV descriptor was overwritten");
                    }
                } else {
                    drop(capture);
                    self.capture_association_fault
                        .store(true, Ordering::Release);
                    self.block("D3D12 capture was armed without a ring during DSV creation");
                }
            }
            Err(poisoned) => {
                drop(poisoned.into_inner());
                self.capture_association_fault
                    .store(true, Ordering::Release);
                self.block("D3D12 capture mutex poisoned during DSV creation");
            }
        }
    }

    fn note_render_camera_contention(&self) {
        self.render_camera_contention_drops
            .fetch_add(1, Ordering::Relaxed);
    }

    fn bound_holstered_production_active(&self) -> bool {
        self.render_camera_production.load(Ordering::Acquire)
            && self.bound_holstered_camera.load(Ordering::Acquire)
            && !crate::kcd2_live_pose::enabled()
    }

    fn production_performance_metrics_enabled(&self) -> bool {
        crate::perf::enabled() && self.bound_holstered_production_active()
    }

    fn lock_render_camera_measured<'a>(
        &'a self,
        measure: bool,
        immediate: &AtomicU64,
        contended: &AtomicU64,
    ) -> std::sync::LockResult<std::sync::MutexGuard<'a, RenderCameraTracker>> {
        if !measure {
            return self.render_camera.lock();
        }
        match self.render_camera.try_lock() {
            Ok(tracker) => {
                immediate.fetch_add(1, Ordering::Relaxed);
                Ok(tracker)
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                contended.fetch_add(1, Ordering::Relaxed);
                self.render_camera.lock()
            }
            Err(std::sync::TryLockError::Poisoned(error)) => Err(error),
        }
    }

    fn render_camera_for_selected_draw(
        &self,
        metrics: Option<&HotListSlot>,
    ) -> Option<std::sync::MutexGuard<'_, RenderCameraTracker>> {
        let tracker = if self.render_camera_production.load(Ordering::Acquire)
            && !self.bound_holstered_camera.load(Ordering::Acquire)
        {
            // Legacy production retains its non-blocking capture contract.
            self.render_camera.try_lock().ok()
        } else {
            // Discovery and bound CP2077 both require lossless observation bookkeeping.
            // Dropping a draw because another callback owns this mutex permanently rejects
            // source authoring or normal StartRun warmup. Serialize those admitted
            // callbacks instead; the mapped bytes are still read in the original draw callback,
            // never deferred to a worker or reconstructed later. This caller holds no other
            // backend guard. Tracker critical sections must not forward game/D3D calls or wait
            // for the GPU. The diagnostic timing cost requires fresh hardware validation.
            if let Some(metrics) = metrics {
                match self.render_camera.try_lock() {
                    Ok(tracker) => {
                        metrics
                            .production_draw_lock_immediate
                            .fetch_add(1, Ordering::Relaxed);
                        Some(tracker)
                    }
                    Err(std::sync::TryLockError::WouldBlock) => {
                        metrics
                            .production_draw_lock_contended
                            .fetch_add(1, Ordering::Relaxed);
                        self.render_camera.lock().ok()
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => None,
                }
            } else {
                self.render_camera.lock().ok()
            }
        };
        if tracker.is_none() {
            self.note_render_camera_contention();
        }
        tracker
    }

    fn render_camera_after_execute(
        &self,
    ) -> std::sync::TryLockResult<std::sync::MutexGuard<'_, RenderCameraTracker>> {
        if self.render_camera_production.load(Ordering::Acquire)
            && !self.bound_holstered_camera.load(Ordering::Acquire)
        {
            self.render_camera.try_lock()
        } else {
            // The pre-Execute upload slices are already owned bytes. Even a submission without
            // a depth-copy fence can carry the selected draw or its camera copy; dropping this
            // commit on WouldBlock creates silent coverage holes. Serialize local observation
            // bookkeeping for discovery and bound CP2077 after the original Execute returned
            // and capture/state guards were released. Do not reread recycled upload memory or
            // weaken generation/queue
            // validation. Legacy production retains its non-blocking policy at the caller.
            self.lock_render_camera_measured(
                self.production_performance_metrics_enabled(),
                &self.camera_execute_complete_lock_immediate,
                &self.camera_execute_complete_lock_contended,
            )
            .map_err(std::sync::TryLockError::Poisoned)
        }
    }
}

fn current_qpc() -> i64 {
    let mut qpc = 0_i64;
    if unsafe { QueryPerformanceCounter(&mut qpc) }.is_ok() {
        qpc
    } else {
        0
    }
}

const fn iunknown_release_index() -> usize {
    core::mem::offset_of!(IUnknown_Vtbl, Release) / core::mem::size_of::<usize>()
}

#[allow(dead_code)]
const fn iunknown_query_interface_index() -> usize {
    core::mem::offset_of!(IUnknown_Vtbl, QueryInterface) / core::mem::size_of::<usize>()
}

const fn factory_create_swapchain_index() -> usize {
    core::mem::offset_of!(IDXGIFactory_Vtbl, CreateSwapChain) / core::mem::size_of::<usize>()
}

const fn factory2_create_swapchain_for_hwnd_index() -> usize {
    core::mem::offset_of!(IDXGIFactory2_Vtbl, CreateSwapChainForHwnd)
        / core::mem::size_of::<usize>()
}

#[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
pub(crate) fn diagnostic_draw_counts() -> [u64; 7] {
    let mut totals = [0; 7];
    if let Some(backend) = backend() {
        for slot in &backend.hot_lists.slots {
            slot.diagnostic_counts.add_to(&mut totals);
        }
    }
    totals
}

fn backend() -> Option<&'static Arc<D3d12ObservationBackend>> {
    ACTIVE_BACKEND.get()
}

unsafe extern "system" fn hook_create_swapchain(
    this: *mut c_void,
    device: *mut c_void,
    desc: *const DXGI_SWAP_CHAIN_DESC,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        factory_create_swapchain_index(),
        HookMethod::CreateSwapChain,
    ) else {
        backend.block("CreateSwapChain original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateSwapChainFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, device, desc, out) };
    if result.is_ok() && !out.is_null() {
        let swapchain = unsafe { *out };
        if let Err(error) = backend.register_swapchain(device, swapchain)
            && error != "DXGI swapchain pDevice is not an ID3D12CommandQueue"
        {
            backend.block(error);
        }
    }
    result
}

unsafe extern "system" fn hook_create_swapchain_for_hwnd(
    this: *mut c_void,
    device: *mut c_void,
    hwnd: HWND,
    desc: *const DXGI_SWAP_CHAIN_DESC1,
    fullscreen: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
    restrict_output: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        factory2_create_swapchain_for_hwnd_index(),
        HookMethod::CreateSwapChainForHwnd,
    ) else {
        backend.block("CreateSwapChainForHwnd original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateSwapChainForHwndFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, device, hwnd, desc, fullscreen, restrict_output, out) };
    if result.is_ok() && !out.is_null() {
        let swapchain = unsafe { *out };
        if let Err(error) = backend.register_swapchain(device, swapchain)
            && error != "DXGI swapchain pDevice is not an ID3D12CommandQueue"
        {
            backend.block(error);
        }
    }
    result
}

unsafe extern "system" fn hook_create_command_queue(
    this: *mut c_void,
    desc: *const D3D12_COMMAND_QUEUE_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_COMMAND_QUEUE,
        HookMethod::CreateCommandQueue,
    ) else {
        backend.block("CreateCommandQueue original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateCommandQueueFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, desc, riid, out) };
    if result.is_ok() && !out.is_null() {
        let object = unsafe { *out };
        let kind = if desc.is_null() {
            backend.block("CreateCommandQueue received a null descriptor");
            D3D12_COMMAND_LIST_TYPE(-1)
        } else {
            unsafe { (*desc).Type }
        };
        if let Ok(mut state) = backend.state.lock() {
            state.register_queue(object as usize, kind);
        }
        let exact_iid = !riid.is_null() && unsafe { *riid == ID3D12CommandQueue::IID };
        if exact_iid && !object.is_null() {
            if let Err(error) = unsafe { backend.install_queue(object) } {
                backend.block(error);
            }
        } else {
            backend.block("CreateCommandQueue did not request ID3D12CommandQueue exactly");
        }
    }
    result
}

unsafe extern "system" fn hook_create_command_list(
    this: *mut c_void,
    node_mask: u32,
    kind: D3D12_COMMAND_LIST_TYPE,
    allocator: *mut c_void,
    initial_state: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_COMMAND_LIST,
        HookMethod::CreateCommandList,
    ) else {
        backend.block("CreateCommandList original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateCommandListFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, node_mask, kind, allocator, initial_state, riid, out) };
    if result.is_ok() && !out.is_null() {
        let object = unsafe { *out };
        let list_tag = backend
            .state
            .lock()
            .ok()
            .and_then(|mut state| state.register_list(object as usize, kind));
        if let Some(list_tag) = list_tag
            && let Ok(mut tracker) = backend.render_camera.lock()
        {
            tracker.register_list(object as usize, list_tag);
            backend.hot_lists.register(object as usize, list_tag);
        }
        backend
            .hot_lists
            .bind_pipeline_state(object as usize, initial_state as usize);
        if let Err(error) = unsafe { validate_graphics_list_base(object, riid) } {
            backend.block(error);
        } else {
            if let Err(error) = unsafe { backend.install_list(object) } {
                backend.block(error);
            }
        }
    }
    result
}

unsafe fn validate_graphics_list_base(
    object: *mut c_void,
    riid: *const GUID,
) -> Result<(), String> {
    if object.is_null() || riid.is_null() {
        return Err("CreateCommandList returned a null object/interface identity".into());
    }
    if unsafe { *riid } == ID3D12GraphicsCommandList::IID {
        return Ok(());
    }
    // Derived GraphicsCommandList interfaces inherit the base vtable. Verify the actual COM
    // pointer before installing base-method hooks; a distinct tear-off pointer is not covered.
    let unknown = unsafe { IUnknown::from_raw_borrowed(&object) }
        .ok_or("CreateCommandList returned an invalid COM object")?;
    let base: ID3D12GraphicsCommandList = unknown
        .cast()
        .map_err(|error| format!("CreateCommandList has no graphics-list base: {error}"))?;
    if base.as_raw() != object {
        return Err("CreateCommandList graphics-list base is a distinct unhooked interface".into());
    }
    Ok(())
}

const MAX_ROOT_SIGNATURE_PARAMETERS: usize = 64;
const MAX_DESCRIPTOR_RANGES_PER_TABLE: usize = 64;
const MAX_DECLARED_CBV_DESCRIPTORS_PER_SIGNATURE: u32 = 256;

struct RootDescriptorRangeDeclaration {
    root_parameter: u32,
    range_type: windows::Win32::Graphics::Direct3D12::D3D12_DESCRIPTOR_RANGE_TYPE,
    descriptor_count: u32,
    declared_offset: u32,
    descriptor_range_index: u32,
    base_shader_register: u32,
    register_space: u32,
}

struct RootCbvRangeBuildContext<'a> {
    output: &'a mut BTreeMap<u32, Vec<RootCbvRange>>,
    table_cursor: &'a mut Option<u32>,
    total_cbv_descriptors: &'a mut u32,
}

fn append_root_cbv_range(
    context: RootCbvRangeBuildContext<'_>,
    declaration: RootDescriptorRangeDeclaration,
) -> Result<(), String> {
    let RootDescriptorRangeDeclaration {
        root_parameter,
        range_type,
        descriptor_count,
        declared_offset,
        descriptor_range_index,
        base_shader_register,
        register_space,
    } = declaration;
    if descriptor_count == 0 {
        return Err("empty root descriptor range is invalid".into());
    }

    // UINT_MAX is the documented D3D12 representation of an unbounded range.
    // It is valid only as the last range in a table, so it makes the table end
    // unknowable but must not make the root-signature object itself unknown.
    // In particular, a bindless SRV/UAV table must not hide exact bounded CBV
    // ranges declared by another root parameter. An unbounded CBV is retained
    // as identity-only evidence: enumerating it would be a broad scan.
    if descriptor_count == u32::MAX {
        *context.table_cursor = None;
        return Ok(());
    }

    let offset = if declared_offset == D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND {
        *context.table_cursor
    } else {
        Some(declared_offset)
    };
    *context.table_cursor = match offset {
        Some(offset) => Some(
            offset
                .checked_add(descriptor_count)
                .ok_or_else(|| "root descriptor table offset overflow".to_string())?,
        ),
        None => None,
    };
    if range_type == D3D12_DESCRIPTOR_RANGE_TYPE_CBV {
        let Some(offset) = offset else {
            // A preceding unbounded range makes APPEND indeterminate. Keep the
            // signature known, but do not guess a physical descriptor offset.
            return Ok(());
        };
        *context.total_cbv_descriptors = context
            .total_cbv_descriptors
            .checked_add(descriptor_count)
            .ok_or_else(|| "root CBV descriptor count overflow".to_string())?;
        if *context.total_cbv_descriptors > MAX_DECLARED_CBV_DESCRIPTORS_PER_SIGNATURE {
            return Err("root signature CBV observation bound exceeded".into());
        }
        context
            .output
            .entry(root_parameter)
            .or_default()
            .push(RootCbvRange {
                table_offset: offset,
                descriptor_count,
                descriptor_range_index,
                base_shader_register,
                register_space,
            });
    }
    Ok(())
}

fn parse_root_signature_cbv_ranges(
    blob: *const c_void,
    blob_bytes: usize,
) -> Result<BTreeMap<u32, Vec<RootCbvRange>>, String> {
    if blob.is_null() || blob_bytes == 0 {
        return Err("root signature blob is null/empty".into());
    }
    let mut raw = std::ptr::null_mut();
    unsafe {
        D3D12CreateVersionedRootSignatureDeserializer(
            blob,
            blob_bytes,
            &ID3D12VersionedRootSignatureDeserializer::IID,
            &mut raw,
        )
    }
    .map_err(|error| format!("root signature deserialization failed: {error}"))?;
    let deserializer = unsafe { ID3D12VersionedRootSignatureDeserializer::from_raw(raw) };
    let versioned =
        unsafe { deserializer.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1) }
            .map_err(|error| format!("root signature 1.1 conversion failed: {error}"))?;
    if versioned.is_null() {
        return Err("root signature deserializer returned null description".into());
    }
    let desc = unsafe { (*versioned).Anonymous.Desc_1_1 };
    if usize::try_from(desc.NumParameters).unwrap_or(usize::MAX) > MAX_ROOT_SIGNATURE_PARAMETERS
        || (desc.NumParameters > 0 && desc.pParameters.is_null())
    {
        return Err("root signature parameter bound/pointer is invalid".into());
    }
    let parameters = if desc.NumParameters == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(desc.pParameters, desc.NumParameters as usize) }
    };
    let mut output = BTreeMap::new();
    let mut total_cbv_descriptors = 0;
    for (root_parameter, parameter) in parameters.iter().enumerate() {
        if parameter.ParameterType != D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
            continue;
        }
        let table = unsafe { parameter.Anonymous.DescriptorTable };
        if usize::try_from(table.NumDescriptorRanges).unwrap_or(usize::MAX)
            > MAX_DESCRIPTOR_RANGES_PER_TABLE
            || (table.NumDescriptorRanges > 0 && table.pDescriptorRanges.is_null())
        {
            return Err("root descriptor-range bound/pointer is invalid".into());
        }
        let ranges = if table.NumDescriptorRanges == 0 {
            &[][..]
        } else {
            unsafe {
                std::slice::from_raw_parts(
                    table.pDescriptorRanges,
                    table.NumDescriptorRanges as usize,
                )
            }
        };
        let mut table_cursor = Some(0);
        for (descriptor_range_index, range) in ranges.iter().enumerate() {
            append_root_cbv_range(
                RootCbvRangeBuildContext {
                    output: &mut output,
                    table_cursor: &mut table_cursor,
                    total_cbv_descriptors: &mut total_cbv_descriptors,
                },
                RootDescriptorRangeDeclaration {
                    root_parameter: root_parameter as u32,
                    range_type: range.RangeType,
                    descriptor_count: range.NumDescriptors,
                    declared_offset: range.OffsetInDescriptorsFromTableStart,
                    descriptor_range_index: descriptor_range_index as u32,
                    base_shader_register: range.BaseShaderRegister,
                    register_space: range.RegisterSpace,
                },
            )?;
        }
    }
    Ok(output)
}

unsafe extern "system" fn hook_create_root_signature(
    this: *mut c_void,
    node_mask: u32,
    blob: *const c_void,
    blob_bytes: usize,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_ROOT_SIGNATURE,
        HookMethod::CreateRootSignature,
    ) else {
        backend.block("CreateRootSignature original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let parsed = parse_root_signature_cbv_ranges(blob, blob_bytes).ok();
    let original: CreateRootSignatureFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, node_mask, blob, blob_bytes, riid, out) };
    if result.is_ok()
        && !riid.is_null()
        && unsafe { *riid == ID3D12RootSignature::IID }
        && !out.is_null()
    {
        let object = unsafe { *out };
        if !object.is_null() {
            if let Some(ranges) = parsed
                && let Ok(mut tracker) = backend.render_camera.lock()
            {
                let root_signature_sha256 = if blob.is_null() || blob_bytes == 0 {
                    None
                } else {
                    let bytes =
                        unsafe { std::slice::from_raw_parts(blob.cast::<u8>(), blob_bytes) };
                    Some(format!("sha256:{:x}", Sha256::digest(bytes)))
                };
                if let Some(root_signature_sha256) = root_signature_sha256 {
                    if let Ok(mut fingerprints) = backend.root_fingerprints.write() {
                        fingerprints.insert(object as usize, root_signature_sha256.clone());
                    }
                    tracker.register_root_signature_with_fingerprint(
                        object as usize,
                        root_signature_sha256,
                        ranges,
                    );
                } else {
                    tracker.register_root_signature(object as usize, ranges);
                }
            }
            if let Err(error) = unsafe { backend.install_root_signature(object) } {
                backend.block(error);
            }
        }
    }
    result
}

unsafe extern "system" fn hook_create_descriptor_heap(
    this: *mut c_void,
    desc: *const D3D12_DESCRIPTOR_HEAP_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_DESCRIPTOR_HEAP,
        HookMethod::CreateDescriptorHeap,
    ) else {
        backend.block("CreateDescriptorHeap original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateDescriptorHeapFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, desc, riid, out) };
    if result.is_err() || desc.is_null() || riid.is_null() || out.is_null() {
        return result;
    }
    let object = unsafe { *out };
    let heap_desc = unsafe { *desc };
    if object.is_null()
        || unsafe { *riid } != ID3D12DescriptorHeap::IID
        || heap_desc.Type != D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV
    {
        return result;
    }
    let Some(heap) = (unsafe { ID3D12DescriptorHeap::from_raw_borrowed(&object) }) else {
        return result;
    };
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&this) }) else {
        return result;
    };
    let cpu_start = unsafe { heap.GetCPUDescriptorHandleForHeapStart() }.ptr as u64;
    let shader_visible = heap_desc.Flags == D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE;
    let gpu_start = if shader_visible {
        unsafe { heap.GetGPUDescriptorHandleForHeapStart() }.ptr
    } else {
        0
    };
    let increment =
        unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) };
    if let Ok(mut tracker) = backend.render_camera.lock() {
        tracker.register_descriptor_heap(
            object as usize,
            cpu_start,
            gpu_start,
            heap_desc.NumDescriptors,
            increment,
            shader_visible,
        );
    }
    if let Err(error) = unsafe { backend.install_descriptor_heap(object) } {
        backend.block(error);
    }
    result
}

unsafe extern "system" fn hook_create_constant_buffer_view(
    this: *mut c_void,
    desc: *const D3D12_CONSTANT_BUFFER_VIEW_DESC,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_CONSTANT_BUFFER_VIEW,
        HookMethod::CreateConstantBufferView,
    ) else {
        backend.block("CreateConstantBufferView original target missing");
        return;
    };
    let original: CreateConstantBufferViewFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, desc, destination) };
    let (gpu_va, bytes) = if desc.is_null() {
        (0, 0)
    } else {
        let desc = unsafe { *desc };
        (desc.BufferLocation, desc.SizeInBytes)
    };
    observe_camera_cbv_descriptor(backend, destination.ptr as u64, gpu_va, bytes);
}

fn observe_camera_cbv_descriptor(
    backend: &D3d12ObservationBackend,
    destination: u64,
    gpu_va: u64,
    bytes: u32,
) {
    // Descriptor identity can be established at game load and reused by a later
    // StartRun. Track writes, copies and invalidation while draw/copy-byte
    // observations are idle too, including overwrites between runs.
    let _timer = backend.descriptor_write_timing.start();
    backend
        .cbv_descriptor_store
        .write_cbv_descriptor(destination, gpu_va, bytes);
}

fn invalidate_camera_descriptor(backend: &D3d12ObservationBackend, destination: u64) {
    let _timer = backend.descriptor_invalidate_timing.start();
    backend
        .cbv_descriptor_store
        .invalidate_cbv_descriptor(destination);
}

unsafe extern "system" fn hook_create_shader_resource_view(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_SHADER_RESOURCE_VIEW,
        HookMethod::CreateShaderResourceView,
    ) else {
        backend.block("CreateShaderResourceView original target missing");
        return;
    };
    let original: CreateShaderResourceViewFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, resource, desc, destination) };
    invalidate_camera_descriptor(backend, destination.ptr as u64);
}

unsafe extern "system" fn hook_create_unordered_access_view(
    this: *mut c_void,
    resource: *mut c_void,
    counter_resource: *mut c_void,
    desc: *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    destination: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_UNORDERED_ACCESS_VIEW,
        HookMethod::CreateUnorderedAccessView,
    ) else {
        backend.block("CreateUnorderedAccessView original target missing");
        return;
    };
    let original: CreateUnorderedAccessViewFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, resource, counter_resource, desc, destination) };
    invalidate_camera_descriptor(backend, destination.ptr as u64);
}

fn descriptor_range_handles(
    starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    sizes: *const u32,
    range_count: u32,
    increment: u32,
) -> Option<Vec<u64>> {
    if range_count == 0 {
        return Some(Vec::new());
    }
    if starts.is_null() || increment == 0 {
        return None;
    }
    let mut handles = Vec::new();
    for range in 0..range_count as usize {
        let count = if sizes.is_null() {
            1
        } else {
            unsafe { *sizes.add(range) }
        };
        if handles.len().saturating_add(count as usize) > 4096 {
            return None;
        }
        let start = unsafe { (*starts.add(range)).ptr as u64 };
        for index in 0..count {
            handles.push(start.checked_add(u64::from(index) * u64::from(increment))?);
        }
    }
    Some(handles)
}

/// Records one copy's batch size: the running total and its histogram bucket.
///
/// The mean alone already produced a 19x error - a back-solve from the
/// 2026-09-10 totals said about 116 descriptors per call, and the 2026-09-11
/// run measured 6.165. A flat distribution and a heavy tail with the same mean
/// need different fixes, so keep the shape, not just the average.
fn note_descriptor_copy_batch(backend: &D3d12ObservationBackend, descriptors: u64) {
    backend
        .descriptor_copy_descriptors
        .fetch_add(descriptors, Ordering::Relaxed);
    let bucket = match descriptors {
        0 | 1 => 0,
        2..=8 => 1,
        9..=32 => 2,
        33..=128 => 3,
        129..=512 => 4,
        _ => 5,
    };
    backend.descriptor_copy_batch_buckets[bucket].fetch_add(1, Ordering::Relaxed);
}

/// One timing sample per copy: sum every sequential shard wait / held-work
/// duration before microsecond truncation. Never count each shard as a copy.
fn apply_descriptor_copy(
    backend: &D3d12ObservationBackend,
    plan: crate::d3d12_render_camera::PreparedCopy<'_>,
) -> crate::d3d12_render_camera::CopyTiming {
    let bucket = match plan.shard_count {
        0 => None,
        1 => Some(0),
        2 => Some(1),
        3..=4 => Some(2),
        5..=8 => Some(3),
        _ => Some(4),
    };
    if let Some(bucket) = bucket {
        backend.descriptor_copy_shard_buckets[bucket].fetch_add(1, Ordering::Relaxed);
    }
    let mut timing = crate::d3d12_render_camera::CopyTiming {
        detail: (crate::perf::diagnostic_epoch() != 0).then(Default::default),
        ..Default::default()
    };
    let result = backend.cbv_descriptor_store.apply_copy(plan, &mut timing);
    if result == Some(false) {
        backend
            .descriptor_copy_untracked
            .fetch_add(1, Ordering::Relaxed);
    }
    backend
        .descriptor_copy_lock_wait_timing
        .record_duration(timing.wait);
    backend
        .descriptor_copy_apply_timing
        .record_duration(timing.apply);
    if let Some(detail) = timing.detail.as_mut() {
        detail.completed = result.is_some();
        detail.wait_ns = crate::execute_prepare::nanos(timing.wait);
        detail.apply_ns = crate::execute_prepare::nanos(timing.apply);
    }
    timing
}

unsafe extern "system" fn hook_copy_descriptors(
    this: *mut c_void,
    destination_range_count: u32,
    destination_starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    destination_sizes: *const u32,
    source_range_count: u32,
    source_starts: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    source_sizes: *const u32,
    heap_type: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::COPY_DESCRIPTORS,
        HookMethod::CopyDescriptors,
    ) else {
        backend.block("CopyDescriptors original target missing");
        return;
    };
    let original: CopyDescriptorsFn = unsafe { std::mem::transmute(target) };
    unsafe {
        original(
            this,
            destination_range_count,
            destination_starts,
            destination_sizes,
            source_range_count,
            source_starts,
            source_sizes,
            heap_type,
        )
    };
    if heap_type != D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV {
        return;
    }
    let timer = backend
        .descriptor_copy_timing
        .start_traced(crate::perf::SlowHookKind::DescriptorCopy);
    let prepare = backend.descriptor_copy_prepare_timing.start();
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&this) }) else {
        return;
    };
    let increment = unsafe { device.GetDescriptorHandleIncrementSize(heap_type) };
    let Some(destinations) = descriptor_range_handles(
        destination_starts,
        destination_sizes,
        destination_range_count,
        increment,
    ) else {
        return;
    };
    let Some(sources) =
        descriptor_range_handles(source_starts, source_sizes, source_range_count, increment)
    else {
        return;
    };
    note_descriptor_copy_batch(backend, destinations.len() as u64);
    let Some(plan) = CbvDescriptorStore::prepare_copy(&destinations, &sources) else {
        return;
    };
    let prepare_elapsed = prepare.finish();
    let mut timing = apply_descriptor_copy(backend, plan);
    if let Some(detail) = timing.detail.as_mut() {
        detail.prepare_ns = crate::execute_prepare::nanos(prepare_elapsed);
    }
    // Keep the parent timing boundary inclusive of the same Vec drops as before.
    drop(sources);
    drop(destinations);
    timer.finish_copy(timing.detail);
}

unsafe extern "system" fn hook_copy_descriptors_simple(
    this: *mut c_void,
    descriptor_count: u32,
    destination_start: D3D12_CPU_DESCRIPTOR_HANDLE,
    source_start: D3D12_CPU_DESCRIPTOR_HANDLE,
    heap_type: D3D12_DESCRIPTOR_HEAP_TYPE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::COPY_DESCRIPTORS_SIMPLE,
        HookMethod::CopyDescriptorsSimple,
    ) else {
        backend.block("CopyDescriptorsSimple original target missing");
        return;
    };
    let original: CopyDescriptorsSimpleFn = unsafe { std::mem::transmute(target) };
    unsafe {
        original(
            this,
            descriptor_count,
            destination_start,
            source_start,
            heap_type,
        )
    };
    if heap_type != D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV || descriptor_count > 4096 {
        return;
    }
    let timer = backend
        .descriptor_copy_timing
        .start_traced(crate::perf::SlowHookKind::DescriptorCopy);
    let prepare = backend.descriptor_copy_prepare_timing.start();
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&this) }) else {
        return;
    };
    let increment = unsafe { device.GetDescriptorHandleIncrementSize(heap_type) };
    let destinations =
        descriptor_range_handles(&destination_start, &descriptor_count, 1, increment);
    let sources = descriptor_range_handles(&source_start, &descriptor_count, 1, increment);
    note_descriptor_copy_batch(backend, u64::from(descriptor_count));
    if let (Some(destinations), Some(sources)) = (destinations, sources) {
        let Some(plan) = CbvDescriptorStore::prepare_copy(&destinations, &sources) else {
            return;
        };
        let prepare_elapsed = prepare.finish();
        let mut timing = apply_descriptor_copy(backend, plan);
        if let Some(detail) = timing.detail.as_mut() {
            detail.prepare_ns = crate::execute_prepare::nanos(prepare_elapsed);
        }
        drop(sources);
        drop(destinations);
        timer.finish_copy(timing.detail);
    }
}

unsafe extern "system" fn hook_create_depth_stencil_view(
    this: *mut c_void,
    resource: *mut c_void,
    desc: *const D3D12_DEPTH_STENCIL_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_DEPTH_STENCIL_VIEW,
        HookMethod::CreateDepthStencilView,
    ) else {
        backend.block("CreateDepthStencilView original target missing");
        return;
    };
    let original: CreateDepthStencilViewFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, resource, desc, handle) };
    let metadata = if resource.is_null() {
        None
    } else {
        let raw = resource;
        let vtable = unsafe { *resource.cast::<*const usize>() };
        let get_desc = if vtable.is_null() {
            0
        } else {
            unsafe { *vtable.add(d3d12_abi::resource::GET_DESC) }
        };
        // A real COM method cannot live in the null/low-address guard pages.
        // This also keeps synthetic partial vtables observation-only.
        let has_get_desc = get_desc >= 0x1_0000;
        has_get_desc
            .then(|| unsafe { ID3D12Resource::from_raw_borrowed(&raw) })
            .flatten()
            .map(|resource| {
                let resource_desc = unsafe { resource.GetDesc() };
                let (dsv_format, dsv_dimension, dsv_flags, view_desc_explicit) = if desc.is_null() {
                    (resource_desc.Format, 0, 0, false)
                } else {
                    let desc = unsafe { &*desc };
                    (desc.Format, desc.ViewDimension.0, desc.Flags.0, true)
                };
                DsvMetadata {
                    resource_dimension: resource_desc.Dimension.0,
                    width: resource_desc.Width,
                    height: resource_desc.Height,
                    depth_or_array_size: resource_desc.DepthOrArraySize,
                    mip_levels: resource_desc.MipLevels,
                    sample_count: resource_desc.SampleDesc.Count,
                    sample_quality: resource_desc.SampleDesc.Quality,
                    resource_format: resource_desc.Format.0,
                    resource_layout: resource_desc.Layout.0,
                    resource_flags: resource_desc.Flags.0,
                    dsv_format: dsv_format.0,
                    dsv_dimension,
                    dsv_flags,
                    view_desc_explicit,
                }
            })
    };
    let new_identity = if let Ok(mut state) = backend.state.lock() {
        state.write_dsv(handle.ptr as u64, resource as usize, metadata);
        state.resolve_handle(handle.ptr as u64)
    } else {
        None
    };
    if backend.capture_armed.load(Ordering::Acquire) {
        backend.update_dsv_capture_after_admission(handle.ptr as u64, new_identity);
    }
    if !resource.is_null()
        && let Err(error) = unsafe { backend.install_resource(resource) }
    {
        backend.block(error);
    }
}

fn observe_created_buffer(
    backend: &D3d12ObservationBackend,
    object: *mut c_void,
    desc: *const D3D12_RESOURCE_DESC,
    riid: *const GUID,
    upload_heap: bool,
) -> Result<(), String> {
    if object.is_null() || desc.is_null() || riid.is_null() {
        return Ok(());
    }
    let resource_desc = unsafe { *desc };
    if resource_desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER {
        return Ok(());
    }
    if unsafe { *riid } != ID3D12Resource::IID {
        // CP2077 requests ID3D12Resource for the observed buffer pool. Returning a newer interface
        // would require keeping an additional canonical-IUnknown alias map; refuse to guess one.
        return Ok(());
    }
    let raw = object;
    let resource = unsafe { ID3D12Resource::from_raw_borrowed(&raw) }
        .ok_or("D3D12 buffer resource pointer is invalid")?;
    let gpu_virtual_address = unsafe { resource.GetGPUVirtualAddress() };
    if let Ok(mut tracker) = backend.render_camera.lock() {
        tracker.register_buffer_with_heap_kind(
            raw as usize,
            gpu_virtual_address,
            resource_desc.Width,
            upload_heap,
        );
    }
    unsafe { backend.install_resource(raw) }
}

fn desc1_as_desc(desc: *const D3D12_RESOURCE_DESC1) -> Option<D3D12_RESOURCE_DESC> {
    let desc = unsafe { desc.as_ref() }?;
    Some(D3D12_RESOURCE_DESC {
        Dimension: desc.Dimension,
        Alignment: desc.Alignment,
        Width: desc.Width,
        Height: desc.Height,
        DepthOrArraySize: desc.DepthOrArraySize,
        MipLevels: desc.MipLevels,
        Format: desc.Format,
        SampleDesc: desc.SampleDesc,
        Layout: desc.Layout,
        Flags: desc.Flags,
    })
}

const MAX_OBSERVER_UPLOAD_MAPPINGS: usize = 64;

fn observe_copy_buffer_resource(
    backend: &D3d12ObservationBackend,
    object: *mut c_void,
    read_range: Option<D3D12_RANGE>,
) -> Result<(), String> {
    if object.is_null() {
        return Ok(());
    }
    let raw = object;
    let already_observed = backend
        .render_camera
        .lock()
        .is_ok_and(|tracker| tracker.has_buffer(raw as usize));
    let resource = unsafe { ID3D12Resource::from_raw_borrowed(&raw) }
        .ok_or("CopyBufferRegion resource pointer is invalid")?;
    if !already_observed {
        let desc = unsafe { resource.GetDesc() };
        if desc.Dimension != D3D12_RESOURCE_DIMENSION_BUFFER {
            return Ok(());
        }
        let mut heap_properties = D3D12_HEAP_PROPERTIES::default();
        let mut heap_flags = D3D12_HEAP_FLAGS::default();
        unsafe { resource.GetHeapProperties(Some(&mut heap_properties), Some(&mut heap_flags)) }
            .map_err(|error| format!("CopyBufferRegion GetHeapProperties failed: {error}"))?;
        let upload_heap = heap_properties.Type == D3D12_HEAP_TYPE_UPLOAD;
        let gpu_virtual_address = unsafe { resource.GetGPUVirtualAddress() };
        if let Ok(mut tracker) = backend.render_camera.lock() {
            tracker.register_buffer_with_heap_kind(
                raw as usize,
                gpu_virtual_address,
                desc.Width,
                upload_heap,
            );
        }
        unsafe { backend.install_resource(raw) }?;
    }
    let Some(read_range) = read_range else {
        return Ok(());
    };
    let needs_map = backend
        .render_camera
        .lock()
        .is_ok_and(|tracker| tracker.buffer_needs_observer_map(raw as usize));
    if !needs_map {
        return Ok(());
    }
    let mut mappings = backend
        .observer_upload_mappings
        .lock()
        .map_err(|_| "observer upload mapping mutex poisoned".to_string())?;
    if mappings.contains_key(&(raw as usize)) || mappings.len() >= MAX_OBSERVER_UPLOAD_MAPPINGS {
        return Ok(());
    }
    let mut mapped = std::ptr::null_mut();
    let empty_read_range = D3D12_RANGE { Begin: 0, End: 0 };
    let owned = resource.clone();
    let map_target = backend
        .original_target(raw, d3d12_abi::resource::MAP, HookMethod::MapResource)
        .ok_or("observer Map original target missing")?;
    let unmap_target = backend
        .original_target(raw, d3d12_abi::resource::UNMAP, HookMethod::UnmapResource)
        .ok_or("observer Unmap original target missing")?;
    let map: MapResourceFn = unsafe { std::mem::transmute(map_target) };
    let unmap: UnmapResourceFn = unsafe { std::mem::transmute(unmap_target) };
    unsafe { map(raw, 0, &read_range, &mut mapped) }
        .ok()
        .map_err(|error| format!("observer upload Map failed: {error}"))?;
    if mapped.is_null() {
        unsafe { unmap(raw, 0, &empty_read_range) };
        return Err("CopyBufferRegion upload Map returned null".into());
    }
    // Observer ownership must not hide the application's next 0->1 Map transition:
    // that transition advances content generation even while our nested map remains alive.
    let registration = backend
        .render_camera
        .lock()
        .map_err(|_| "observer mapping tracker poisoned".to_owned())
        .and_then(|mut tracker| tracker.register_observer_mapping(raw as usize, mapped as usize));
    if let Err(error) = registration {
        unsafe { unmap(raw, 0, &empty_read_range) };
        return Err(error);
    }
    mappings.insert(raw as usize, owned);
    Ok(())
}

unsafe extern "system" fn hook_create_committed_resource(
    this: *mut c_void,
    heap_properties: *const D3D12_HEAP_PROPERTIES,
    heap_flags: D3D12_HEAP_FLAGS,
    desc: *const D3D12_RESOURCE_DESC,
    initial_state: D3D12_RESOURCE_STATES,
    optimized_clear_value: *const D3D12_CLEAR_VALUE,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_COMMITTED_RESOURCE,
        HookMethod::CreateCommittedResource,
    ) else {
        backend.block("CreateCommittedResource original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateCommittedResourceFn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap_properties,
            heap_flags,
            desc,
            initial_state,
            optimized_clear_value,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            desc,
            riid,
            !heap_properties.is_null()
                && unsafe { (*heap_properties).Type == D3D12_HEAP_TYPE_UPLOAD },
        )
    {
        backend.block(error);
    }
    result
}

unsafe extern "system" fn hook_create_placed_resource(
    this: *mut c_void,
    heap: *mut c_void,
    heap_offset: u64,
    desc: *const D3D12_RESOURCE_DESC,
    initial_state: D3D12_RESOURCE_STATES,
    optimized_clear_value: *const D3D12_CLEAR_VALUE,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device::CREATE_PLACED_RESOURCE,
        HookMethod::CreatePlacedResource,
    ) else {
        backend.block("CreatePlacedResource original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreatePlacedResourceFn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap,
            heap_offset,
            desc,
            initial_state,
            optimized_clear_value,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            desc,
            riid,
            unsafe { ID3D12Heap::from_raw_borrowed(&heap) }.is_some_and(|heap| unsafe {
                heap.GetDesc().Properties.Type == D3D12_HEAP_TYPE_UPLOAD
            }),
        )
    {
        backend.block(error);
    }
    result
}

#[allow(dead_code)]
unsafe extern "system" fn hook_create_committed_resource1(
    this: *mut c_void,
    heap_properties: *const D3D12_HEAP_PROPERTIES,
    heap_flags: D3D12_HEAP_FLAGS,
    desc: *const D3D12_RESOURCE_DESC,
    initial_state: D3D12_RESOURCE_STATES,
    clear: *const D3D12_CLEAR_VALUE,
    protected_session: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device4::CREATE_COMMITTED_RESOURCE1,
        HookMethod::CreateCommittedResource1,
    ) else {
        backend.block("CreateCommittedResource1 original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateCommittedResource1Fn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap_properties,
            heap_flags,
            desc,
            initial_state,
            clear,
            protected_session,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            desc,
            riid,
            !heap_properties.is_null()
                && unsafe { (*heap_properties).Type == D3D12_HEAP_TYPE_UPLOAD },
        )
    {
        backend.block(error);
    }
    result
}

#[allow(dead_code)]
unsafe extern "system" fn hook_create_committed_resource2(
    this: *mut c_void,
    heap_properties: *const D3D12_HEAP_PROPERTIES,
    heap_flags: D3D12_HEAP_FLAGS,
    desc1: *const D3D12_RESOURCE_DESC1,
    initial_state: D3D12_RESOURCE_STATES,
    clear: *const D3D12_CLEAR_VALUE,
    protected_session: *mut c_void,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device8::CREATE_COMMITTED_RESOURCE2,
        HookMethod::CreateCommittedResource2,
    ) else {
        backend.block("CreateCommittedResource2 original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateCommittedResource2Fn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap_properties,
            heap_flags,
            desc1,
            initial_state,
            clear,
            protected_session,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Some(desc) = desc1_as_desc(desc1)
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            &desc,
            riid,
            !heap_properties.is_null()
                && unsafe { (*heap_properties).Type == D3D12_HEAP_TYPE_UPLOAD },
        )
    {
        backend.block(error);
    }
    result
}

#[allow(dead_code)]
unsafe extern "system" fn hook_create_placed_resource1(
    this: *mut c_void,
    heap: *mut c_void,
    heap_offset: u64,
    desc1: *const D3D12_RESOURCE_DESC1,
    initial_state: D3D12_RESOURCE_STATES,
    clear: *const D3D12_CLEAR_VALUE,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device8::CREATE_PLACED_RESOURCE1,
        HookMethod::CreatePlacedResource1,
    ) else {
        backend.block("CreatePlacedResource1 original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreatePlacedResource1Fn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap,
            heap_offset,
            desc1,
            initial_state,
            clear,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Some(desc) = desc1_as_desc(desc1)
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            &desc,
            riid,
            unsafe { ID3D12Heap::from_raw_borrowed(&heap) }.is_some_and(|heap| unsafe {
                heap.GetDesc().Properties.Type == D3D12_HEAP_TYPE_UPLOAD
            }),
        )
    {
        backend.block(error);
    }
    result
}

#[allow(dead_code)]
unsafe extern "system" fn hook_create_committed_resource3(
    this: *mut c_void,
    heap_properties: *const D3D12_HEAP_PROPERTIES,
    heap_flags: D3D12_HEAP_FLAGS,
    desc1: *const D3D12_RESOURCE_DESC1,
    initial_layout: windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT,
    clear: *const D3D12_CLEAR_VALUE,
    protected_session: *mut c_void,
    format_count: u32,
    formats: *const DXGI_FORMAT,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device10::CREATE_COMMITTED_RESOURCE3,
        HookMethod::CreateCommittedResource3,
    ) else {
        backend.block("CreateCommittedResource3 original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreateCommittedResource3Fn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap_properties,
            heap_flags,
            desc1,
            initial_layout,
            clear,
            protected_session,
            format_count,
            formats,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Some(desc) = desc1_as_desc(desc1)
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            &desc,
            riid,
            !heap_properties.is_null()
                && unsafe { (*heap_properties).Type == D3D12_HEAP_TYPE_UPLOAD },
        )
    {
        backend.block(error);
    }
    result
}

#[allow(dead_code)]
unsafe extern "system" fn hook_create_placed_resource2(
    this: *mut c_void,
    heap: *mut c_void,
    heap_offset: u64,
    desc1: *const D3D12_RESOURCE_DESC1,
    initial_layout: windows::Win32::Graphics::Direct3D12::D3D12_BARRIER_LAYOUT,
    clear: *const D3D12_CLEAR_VALUE,
    format_count: u32,
    formats: *const DXGI_FORMAT,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::device10::CREATE_PLACED_RESOURCE2,
        HookMethod::CreatePlacedResource2,
    ) else {
        backend.block("CreatePlacedResource2 original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: CreatePlacedResource2Fn = unsafe { std::mem::transmute(target) };
    let result = unsafe {
        original(
            this,
            heap,
            heap_offset,
            desc1,
            initial_layout,
            clear,
            format_count,
            formats,
            riid,
            out,
        )
    };
    if result.is_ok()
        && !out.is_null()
        && let Some(desc) = desc1_as_desc(desc1)
        && let Err(error) = observe_created_buffer(
            backend,
            unsafe { *out },
            &desc,
            riid,
            unsafe { ID3D12Heap::from_raw_borrowed(&heap) }.is_some_and(|heap| unsafe {
                heap.GetDesc().Properties.Type == D3D12_HEAP_TYPE_UPLOAD
            }),
        )
    {
        backend.block(error);
    }
    result
}

unsafe extern "system" fn hook_execute_command_lists(
    this: *mut c_void,
    count: u32,
    lists: *const *mut c_void,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::command_queue::EXECUTE_COMMAND_LISTS,
        HookMethod::ExecuteCommandLists,
    ) else {
        backend.block("ExecuteCommandLists original target missing");
        return;
    };
    let original: ExecuteCommandListsFn = unsafe { std::mem::transmute(target) };
    if count > 0 && lists.is_null() {
        backend.block("ExecuteCommandLists received a null list array");
        return;
    }
    if !backend.runtime_observation_active() {
        unsafe { original(this, count, lists) };
        return;
    }
    // Include pre-prepare queue/tracker admission and post-complete cleanup.
    // Nested slow scopes are time witnesses, never additive CPU accounting.
    let _execute_scope = crate::hook_timing::slow_scope(crate::perf::SlowHookKind::ExecuteHook);
    let submitted = if count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(lists, count as usize) }
    };
    let list_keys: Vec<_> = submitted.iter().map(|list| *list as usize).collect();
    let bound_production_camera = backend.bound_holstered_production_active();
    let production_camera_work =
        bound_production_camera && backend.hot_lists.has_production_camera_work(&list_keys);
    // Use the reader's mode snapshot: reloading the mode during activation could enable
    // parent read counters while the legacy reader branch has no per-read metrics.
    let production_metrics = crate::perf::enabled() && bound_production_camera;
    let queue_id_before_execute = backend.state.lock().ok().and_then(|state| {
        state
            .queues
            .get(&(this as usize))
            .filter(|record| record.kind == D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map(|record| record.id)
    });
    // Snapshot only exact descriptor-proven upload slices before forwarding
    // Execute. The game may recycle persistently mapped upload memory as soon
    // as Execute returns, while the GPU consumes the bytes asynchronously.
    let prepare_timer = backend
        .camera_execute_prepare_timing
        .start_traced(crate::perf::SlowHookKind::ExecutePrepare);
    let mut prepare_sample = crate::execute_prepare::PrepareSample {
        lists: list_keys.len() as u64,
        ..Default::default()
    };
    let mut read_sample = crate::execute_prepare::PrepareSample::default();
    let prepared_copy_shadows = queue_id_before_execute
        .filter(|_| {
            !crate::kcd2_live_pose::enabled()
                && (!bound_production_camera || production_camera_work)
        })
        .map_or_else(Vec::new, |_| {
            // Observer-owned sources are read fallibly without any page query, so there is
            // nothing to prefetch unless the manual A/B switch restores the query path.
            let owned_query_path = mapped_snapshot::owned_query_path_requested();
            let mut query_cache = if bound_production_camera && owned_query_path {
                let Some(cache) = execute_query_prefetch::prefetch(
                    backend,
                    &list_keys,
                    production_metrics,
                    &mut prepare_sample,
                ) else {
                    return Vec::new();
                };
                cache
            } else {
                mapped_snapshot::QueryCache::default()
            };
            let start = production_metrics.then(std::time::Instant::now);
            let locked = backend.lock_render_camera_measured(
                production_metrics,
                &backend.camera_execute_prepare_lock_immediate,
                &backend.camera_execute_prepare_lock_contended,
            );
            prepare_sample.outer_lock_wait_ns += crate::execute_prepare::elapsed(start);
            match locked {
                Ok(mut tracker) => {
                    let held = production_metrics.then(std::time::Instant::now);
                    let held_scope = bound_production_camera.then(|| {
                        crate::hook_timing::slow_scope(
                            crate::perf::SlowHookKind::ExecutePrepareLocked,
                        )
                    });
                    let start = production_metrics.then(std::time::Instant::now);
                    let mapped = tracker.exact_copy_mapped_ranges(&list_keys);
                    let query_owners = bound_production_camera.then(|| {
                        mapped
                            .iter()
                            .filter_map(|(&raw, &(base, _))| {
                                tracker
                                    .observer_mapped_identity(raw, base)
                                    .map(|id| (raw, id))
                            })
                            .collect::<BTreeMap<_, _>>()
                    });
                    prepare_sample.mapped_ranges_ns += crate::execute_prepare::elapsed(start);
                    // Fresh identities after unlocked page queries. Changed/new identities miss
                    // the cache and keep the original query path; borrowed Maps never prefetch.
                    // Per-request list/copy/descriptor validation and all byte reads stay locked.
                    let prepared = {
                        tracker.prepare_exact_cbv_copy_shadows_measured(
                            &list_keys,
                            production_metrics.then_some(&mut prepare_sample),
                            |raw, offset, bytes| {
                                let (base, resource_bytes) = mapped.get(&raw).copied()?;
                                if offset.checked_add(bytes as u64)? > resource_bytes {
                                    return None;
                                }
                                let address = base.checked_add(usize::try_from(offset).ok()?)?;
                                // Keep the original timing/bounds and Unmap retirement guard. Owned
                                // mappings read fallibly with no page query; borrowed mappings
                                // keep the original fresh-query behavior and are explicitly counted.
                                if !bound_production_camera {
                                    return unsafe {
                                        mapped_snapshot::read(address, bytes, false, None)
                                    };
                                }
                                let owner =
                                    query_owners.as_ref().and_then(|ids| ids.get(&raw)).copied();
                                let metrics = production_metrics.then_some(&mut read_sample);
                                match owner {
                                    Some(owner) if !owned_query_path => {
                                        mapped_snapshot::read_owned(owner, address, bytes, metrics)
                                    }
                                    _ => unsafe {
                                        query_cache.read(owner, address, bytes, true, metrics)
                                    },
                                }
                            },
                        )
                    };
                    drop(tracker);
                    prepare_sample.outer_lock_held_ns += crate::execute_prepare::elapsed(held);
                    drop(held_scope);
                    prepared
                }
                Err(_) => {
                    backend.block("D3D12 render-camera tracker poisoned preparing copy provenance");
                    Vec::new()
                }
            }
        });
    if production_metrics && !prepared_copy_shadows.is_empty() {
        backend
            .camera_execute_with_prepared_copy_shadows
            .fetch_add(1, Ordering::Relaxed);
        backend.camera_execute_prepared_copy_shadows.fetch_add(
            prepared_copy_shadows.len().try_into().unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
    if crate::kcd2_live_pose::requires_pre_execute_snapshot() {
        let result = match backend.render_camera.lock() {
            Ok(mut tracker) => tracker.prepare_kcd2_pre_execute_snapshots(
                &list_keys,
                queue_id_before_execute,
                |address, bytes| {
                    if !readable_mapped_range(address, bytes) {
                        return None;
                    }
                    Some(
                        (0..bytes)
                            .map(|index| unsafe {
                                std::ptr::read_volatile((address + index) as *const u8)
                            })
                            .collect(),
                    )
                },
                crate::kcd2_live_pose::current_sample_qpc,
            ),
            Err(error) if error.get_ref().has_kcd2_pre_execute_work(&list_keys) => {
                Err("KCD2 pre-Execute camera tracker is poisoned".to_owned())
            }
            Err(_) => Ok(()),
        };
        if let Err(error) = result {
            // Collector failure must not suppress the game's real command submission.
            crate::kcd2_live_pose::record_submission_fault(error.clone());
            backend.block(error);
        }
    }
    if production_metrics {
        prepare_sample.include_reads(read_sample);
        let sample = prepare_timer.finish_prepare(prepare_sample);
        backend.camera_execute_prepare_details.record(sample);
    } else {
        drop(prepare_timer);
    }
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    let prepared_copy_shadows = {
        let mut prepared = prepared_copy_shadows;
        if bound_production_camera && let Some(queue) = queue_id_before_execute {
            // The tracker guard and existing prepare timing have ended. Diagnostic RPM is
            // separately timestamped, never attributed to VirtualQuery/locked byte reads.
            crate::cp2077_camera_diagnostics::early_player::sample_after_prepare(
                queue.0,
                &mut prepared,
            );
        }
        prepared
    };
    {
        let _original_scope =
            crate::hook_timing::slow_scope(crate::perf::SlowHookKind::ExecuteOriginal);
        unsafe { original(this, count, lists) };
    }
    let _complete_timer = backend
        .camera_execute_complete_timing
        .start_traced(crate::perf::SlowHookKind::ExecuteComplete);
    let mut submitted_depth_present_seqs = Vec::new();
    if backend.capture_armed.load(Ordering::Acquire)
        && let Some(queue) = unsafe { ID3D12CommandQueue::from_raw_borrowed(&this) }
    {
        // Execute is the only point after the application's submission where the
        // private fence can be attached to an injected copy.  Missing this
        // association is unrecoverable, so wait for the short CPU-side drain
        // critical section instead of treating ordinary mutex contention as a
        // corrupt GPU submission.
        match backend.capture.lock() {
            Ok(mut capture) => {
                if let Some(capture) = capture.as_mut() {
                    match capture.on_execute_complete(queue, submitted) {
                        Ok(present_seqs) => submitted_depth_present_seqs = present_seqs,
                        Err(error) => backend.block(error),
                    }
                }
            }
            Err(_) => backend.block("D3D12 capture mutex poisoned during Execute association"),
        }
    } else if backend.capture_armed.load(Ordering::Acquire) {
        backend.block("ExecuteCommandLists queue pointer is invalid");
    }
    let queue_id = if let Ok(mut state) = backend.state.lock() {
        let queue_id = state
            .queues
            .get(&(this as usize))
            .filter(|record| record.kind == D3D12_COMMAND_LIST_TYPE_DIRECT)
            .map(|record| record.id);
        state.execute(this as usize, submitted);
        queue_id
    } else {
        None
    };
    if let Some(queue_id) = queue_id {
        if queue_id_before_execute != Some(queue_id) {
            backend.block("D3D12 render-camera queue identity changed across Execute");
            return;
        }
        let has_copy_proof = !submitted_depth_present_seqs.is_empty();
        if production_metrics && has_copy_proof {
            backend
                .camera_execute_with_depth_copy_proof
                .fetch_add(1, Ordering::Relaxed);
        }
        if bound_production_camera && !production_camera_work && !has_copy_proof {
            // The command lists contain no Draw that staged camera/copy provenance and this
            // submission did not carry the selected depth copy. State.execute above still runs;
            // only the unrelated process-wide camera tracker is bypassed.
            if production_metrics {
                backend
                    .camera_execute_skipped_no_camera_work
                    .fetch_add(1, Ordering::Relaxed);
            }
        } else {
            // KCD2 retains its required blocking submission proof; CP2077 uses
            // the upstream production/discovery contention policy.
            let tracker = if crate::kcd2_live_pose::enabled() {
                backend
                    .render_camera
                    .lock()
                    .map_err(std::sync::TryLockError::Poisoned)
            } else {
                backend.render_camera_after_execute()
            };
            match tracker {
                Ok(mut tracker) => {
                    let has_camera_work =
                        has_copy_proof || tracker.has_pending_for_lists(&list_keys);
                    if production_metrics && has_camera_work {
                        backend
                            .camera_execute_with_camera_work
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    tracker.commit_exact_cbv_copy_shadows(queue_id, prepared_copy_shadows);
                    tracker.materialize_deferred_copy_draws(queue_id, &list_keys);
                    tracker.execute(queue_id, &list_keys);
                    for (present_seq, copy_list, draws_before_in_list) in
                        submitted_depth_present_seqs
                    {
                        let copy_list_index = list_keys
                            .iter()
                            .position(|raw| *raw == copy_list)
                            .and_then(|index| u32::try_from(index).ok());
                        tracker.mark_depth_copy_submitted_in_list(
                            queue_id,
                            present_seq,
                            Some(copy_list),
                            copy_list_index,
                            draws_before_in_list,
                        );
                    }
                    if bound_production_camera {
                        backend.hot_lists.clear_production_camera_work(&list_keys);
                    }
                }
                Err(std::sync::TryLockError::WouldBlock) if !has_copy_proof => {}
                Err(std::sync::TryLockError::WouldBlock) => {
                    backend.block("D3D12 render-camera tracker busy for depth-copy proof")
                }
                Err(std::sync::TryLockError::Poisoned(_)) => {
                    backend.block("D3D12 render-camera tracker poisoned at Execute")
                }
            }
        }
    }
}

unsafe extern "system" fn hook_release_queue(this: *mut c_void) -> u32 {
    let Some(backend) = backend() else {
        return 0;
    };
    let Some(target) =
        backend.original_target(this, iunknown_release_index(), HookMethod::ReleaseQueue)
    else {
        backend.block("ID3D12CommandQueue::Release original target missing");
        return 0;
    };
    let original: ReleaseFn = unsafe { std::mem::transmute(target) };
    let remaining = unsafe { original(this) };
    if remaining == 0
        && let Ok(mut state) = backend.state.lock()
    {
        state.queues.remove(&(this as usize));
    }
    remaining
}

unsafe extern "system" fn hook_release_list(this: *mut c_void) -> u32 {
    let Some(backend) = backend() else {
        return 0;
    };
    let Some(target) =
        backend.original_target(this, iunknown_release_index(), HookMethod::ReleaseList)
    else {
        backend.block("ID3D12GraphicsCommandList::Release original target missing");
        return 0;
    };
    let original: ReleaseFn = unsafe { std::mem::transmute(target) };
    let remaining = unsafe { original(this) };
    if remaining == 0 {
        backend.hot_lists.release(this as usize);
        backend.discard_recorded_capture_list(this as usize, "Release");
        if let Ok(mut state) = backend.state.lock() {
            state.lists.remove(&(this as usize));
        }
        if let Ok(mut tracker) = backend.render_camera.lock() {
            tracker.release_list(this as usize);
        }
    }
    remaining
}

unsafe extern "system" fn hook_release_resource(this: *mut c_void) -> u32 {
    let Some(backend) = backend() else {
        return 0;
    };
    let Some(target) =
        backend.original_target(this, iunknown_release_index(), HookMethod::ReleaseResource)
    else {
        backend.block("ID3D12Resource::Release original target missing");
        return 0;
    };
    let original: ReleaseFn = unsafe { std::mem::transmute(target) };
    let remaining = unsafe { original(this) };
    if remaining == 0
        && let Ok(mut state) = backend.state.lock()
    {
        state.release_resource(this as usize);
    }
    if remaining == 0
        && let Ok(mut tracker) = backend.render_camera.lock()
    {
        tracker.release_resource(this as usize);
    }
    remaining
}

unsafe extern "system" fn hook_release_descriptor_heap(this: *mut c_void) -> u32 {
    let Some(backend) = backend() else {
        return 0;
    };
    let Some(target) = backend.original_target(
        this,
        iunknown_release_index(),
        HookMethod::ReleaseDescriptorHeap,
    ) else {
        backend.block("ID3D12DescriptorHeap::Release original target missing");
        return 0;
    };
    let original: ReleaseFn = unsafe { std::mem::transmute(target) };
    let remaining = unsafe { original(this) };
    if remaining == 0
        && let Ok(mut tracker) = backend.render_camera.lock()
    {
        backend
            .descriptor_heap_releases
            .fetch_add(1, Ordering::Relaxed);
        tracker.release_descriptor_heap(this as usize);
    }
    remaining
}

unsafe extern "system" fn hook_release_root_signature(this: *mut c_void) -> u32 {
    let Some(backend) = backend() else {
        return 0;
    };
    let Some(target) = backend.original_target(
        this,
        iunknown_release_index(),
        HookMethod::ReleaseRootSignature,
    ) else {
        backend.block("ID3D12RootSignature::Release original target missing");
        return 0;
    };
    let original: ReleaseFn = unsafe { std::mem::transmute(target) };
    let remaining = unsafe { original(this) };
    if remaining == 0
        && let Ok(mut tracker) = backend.render_camera.lock()
    {
        tracker.release_root_signature(this as usize);
        if let Ok(mut fingerprints) = backend.root_fingerprints.write() {
            fingerprints.remove(&(this as usize));
        }
    }
    remaining
}

unsafe extern "system" fn hook_reset(
    this: *mut c_void,
    allocator: *mut c_void,
    initial_state: *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::RESET,
        HookMethod::Reset,
    ) else {
        backend.block("Reset original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: ResetFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, allocator, initial_state) };
    if !backend.runtime_observation_active() {
        if result.is_ok() {
            backend
                .hot_lists
                .bind_pipeline_state(this as usize, initial_state as usize);
        }
        return result;
    }
    if result.is_ok() {
        backend
            .hot_lists
            .reset_with_pipeline(this as usize, initial_state as usize);
        backend.discard_recorded_capture_list(this as usize, "Reset");
        if let Ok(mut state) = backend.state.lock() {
            state.reset_list(this as usize);
        }
        if let Ok(mut tracker) = backend.render_camera.lock() {
            tracker.reset_list(this as usize);
        }
    }
    result
}

unsafe extern "system" fn hook_rs_set_viewports(
    this: *mut c_void,
    count: u32,
    viewports: *const windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::RS_SET_VIEWPORTS,
        HookMethod::RsSetViewports,
    ) else {
        backend.block("RSSetViewports original target missing");
        return;
    };
    let original: unsafe extern "system" fn(
        *mut c_void,
        u32,
        *const windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT,
    ) = unsafe { std::mem::transmute(target) };
    unsafe { original(this, count, viewports) };
    if !backend.runtime_observation_active() {
        return;
    }
    // Viewport participates in the frozen draw-pass proof. Repeated setter calls conservatively
    // invalidate the exact-Draw cache; CP2077 still issues far fewer of these than Draw calls.
    backend.hot_lists.touch_production_state(this as usize);
    let first = if count == 1 && !viewports.is_null() {
        unsafe { *viewports }
    } else {
        Default::default()
    };
    if let Ok(mut tracker) = backend.render_camera.lock() {
        tracker.set_viewport(
            this as usize,
            crate::d3d12_render_camera::CapturedViewport { count, first },
        );
    } else {
        backend.block("D3D12 viewport tracker poisoned");
    }
}

unsafe extern "system" fn hook_rs_set_scissor_rects(
    this: *mut c_void,
    count: u32,
    rects: *const RECT,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::RS_SET_SCISSOR_RECTS,
        HookMethod::RsSetScissorRects,
    ) else {
        backend.block("RSSetScissorRects original target missing");
        return;
    };
    let original: RsSetScissorRectsFn = unsafe { std::mem::transmute(target) };
    unsafe { scissor_after_original(backend, original, this, count, rects) };
}

unsafe fn scissor_after_original(
    backend: &D3d12ObservationBackend,
    original: RsSetScissorRectsFn,
    this: *mut c_void,
    count: u32,
    rects: *const RECT,
) {
    unsafe { original(this, count, rects) };
    if !backend.runtime_observation_active() || !backend.draw_pass_evidence_enabled() {
        return;
    }
    // Scissor is also part of the frozen pass context. Do not represent a Draw across a setter.
    backend.hot_lists.touch_production_state(this as usize);
    let _timer = backend.camera_scissor_timing.start();
    let single_rect = if count == 1 && readable_mapped_range(rects as usize, size_of::<RECT>()) {
        let rect = unsafe { std::ptr::read_unaligned(rects) };
        Some([rect.left, rect.top, rect.right, rect.bottom])
    } else {
        None
    };
    if let Ok(mut tracker) = backend.render_camera.lock() {
        tracker.set_scissor(
            this as usize,
            crate::d3d12_render_camera::draw_pass::ScissorObservation { count, single_rect },
        );
    } else {
        backend.note_render_camera_contention();
    }
}

unsafe extern "system" fn hook_map_resource(
    this: *mut c_void,
    subresource: u32,
    read_range: *const D3D12_RANGE,
    data: *mut *mut c_void,
) -> HRESULT {
    let Some(backend) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) =
        backend.original_target(this, d3d12_abi::resource::MAP, HookMethod::MapResource)
    else {
        backend.block("ID3D12Resource::Map original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: MapResourceFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, subresource, read_range, data) };
    let _timer = backend.resource_map_timing.start();
    if result.is_ok() && subresource == 0 && !data.is_null() {
        let mapped = unsafe { *data } as usize;
        if let Ok(mut tracker) = backend.render_camera.lock() {
            tracker.map_buffer(this as usize, mapped);
        }
    }
    result
}

unsafe extern "system" fn hook_unmap_resource(
    this: *mut c_void,
    subresource: u32,
    written_range: *const D3D12_RANGE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) =
        backend.original_target(this, d3d12_abi::resource::UNMAP, HookMethod::UnmapResource)
    else {
        backend.block("ID3D12Resource::Unmap original target missing");
        return;
    };
    let original: UnmapResourceFn = unsafe { std::mem::transmute(target) };
    if !backend.runtime_observation_active() {
        if subresource == 0
            && let Ok(mut tracker) = backend.render_camera.lock()
        {
            tracker.unmap_buffer(this as usize);
        }
        unsafe { original(this, subresource, written_range) };
        return;
    }
    let timer = backend.resource_unmap_timing.start();
    if subresource == 0
        && let Ok(mut tracker) = backend.render_camera.lock()
    {
        let written_range = (!written_range.is_null()).then(|| {
            let range = unsafe { *written_range };
            (range.Begin, range.End)
        });
        // KCD2's pinned sources are Direct root-CBV reads while mapped. They never consume
        // descriptor-table Unmap shadows. Avoid broad reads of write-combined uploads
        // that cannot contribute to this binding's camera/pose evidence.
        if !crate::kcd2_live_pose::enabled() {
            tracker.snapshot_upload_cbvs_before_final_unmap(
                this as usize,
                written_range,
                |address, bytes| {
                    if !readable_mapped_range(address, bytes) {
                        return None;
                    }
                    Some(
                        (0..bytes)
                            .map(|index| unsafe {
                                std::ptr::read_volatile((address + index) as *const u8)
                            })
                            .collect(),
                    )
                },
            );
        }
        // Retire the observer's mapped pointer before forwarding Unmap. This
        // deliberately creates a tiny observation-loss window, never a window
        // in which another draw callback can dereference memory after the
        // runtime has invalidated the pointer.
        tracker.unmap_buffer(this as usize);
    }
    drop(timer);
    unsafe { original(this, subresource, written_range) };
}

unsafe extern "system" fn hook_resource_barrier(
    this: *mut c_void,
    count: u32,
    barriers: *const D3D12_RESOURCE_BARRIER,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::RESOURCE_BARRIER,
        HookMethod::ResourceBarrier,
    ) else {
        backend.block("ResourceBarrier original target missing");
        return;
    };
    let original: ResourceBarrierFn = unsafe { std::mem::transmute(target) };
    if count > 0 && barriers.is_null() {
        backend.block("ResourceBarrier received a null barrier array");
        return;
    }
    if !backend.runtime_observation_active() {
        unsafe { original(this, count, barriers) };
        return;
    }
    let barrier_slice = if count == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(barriers, count as usize) }
    };
    // Capture before forwarding the application's transition. CP2077's selected
    // scene depth is transient: waiting until the following Clear can observe
    // aliased/undefined bytes even though the descriptor identity is stable.
    for barrier in barrier_slice {
        if barrier.Type == D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
            let transition = unsafe { &*barrier.Anonymous.Transition };
            backend.capture_before_depth_write_exit(this, transition, barrier.Flags, target);
        }
    }
    unsafe { original(this, count, barriers) };
    if let Ok(mut state) = backend.state.lock() {
        state.resource_barrier_calls += 1;
        for barrier in barrier_slice {
            if barrier.Type != D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
                continue;
            }
            let transition = unsafe { &*barrier.Anonymous.Transition };
            let resource = transition
                .pResource
                .as_ref()
                .map_or(0, |value| Interface::as_raw(value) as usize);
            let raw = RawTransition {
                resource,
                before: transition.StateBefore,
                after: transition.StateAfter,
                subresource: transition.Subresource,
                split: barrier.Flags == D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY
                    || barrier.Flags == D3D12_RESOURCE_BARRIER_FLAG_END_ONLY,
            };
            state.record_transition_state(this as usize, raw);
            state.record_event(this as usize, ListEvent::Transition(raw), "ResourceBarrier");
        }
    }
    if backend.depth_order_diagnostics.load(Ordering::Acquire)
        && crate::raw_depth_demo::authoring_camera_interval_active()
    {
        for barrier in barrier_slice {
            if barrier.Type != D3D12_RESOURCE_BARRIER_TYPE_TRANSITION {
                continue;
            }
            let transition = unsafe { &*barrier.Anonymous.Transition };
            let resource = transition
                .pResource
                .as_ref()
                .map_or(std::ptr::null_mut(), |r| r.as_raw());
            if resource as usize
                != backend
                    .diagnostic_depth_resource_raw
                    .load(Ordering::Relaxed)
            {
                continue;
            }
            let identity = backend.capture.lock().ok().and_then(|capture| {
                capture
                    .as_ref()
                    .and_then(|ring| ring.diagnostic_interval(resource))
                    .map(|v| v.0)
            });
            if let Some(identity) = identity
                && let Ok(mut tracker) = backend.render_camera.lock()
            {
                tracker.record_depth_boundary(
                    this as usize,
                    identity,
                    backend.hot_lists.recorded_draw_count(this as usize),
                    crate::d3d12_render_camera::depth_order::BoundaryKind::Transition {
                        before: transition.StateBefore.0 as u32,
                        after: transition.StateAfter.0 as u32,
                        subresource: transition.Subresource,
                        flags: barrier.Flags.0 as u32,
                    },
                );
            }
        }
    }
}

unsafe extern "system" fn hook_copy_buffer_region(
    this: *mut c_void,
    destination: *mut c_void,
    destination_offset: u64,
    source: *mut c_void,
    source_offset: u64,
    bytes: u64,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::COPY_BUFFER_REGION,
        HookMethod::CopyBufferRegion,
    ) else {
        backend.block("CopyBufferRegion original target missing");
        return;
    };
    let original: CopyBufferRegionFn = unsafe { std::mem::transmute(target) };
    unsafe {
        original(
            this,
            destination,
            destination_offset,
            source,
            source_offset,
            bytes,
        )
    };
    if backend.runtime_observation_active() {
        // Any buffer-copy command can replace the physical bytes observed by later Draws on this
        // list. It invalidates redundancy even when no selected DSV is currently bound.
        backend.hot_lists.touch_production_state(this as usize);
    }
    // Resource provenance is needed only for an explicitly armed local raw
    // capture. Discovery has no selected DSV identity, and walking every copy
    // during game startup would add high mutex/COM overhead without being able
    // to produce a camera observation.
    if backend.selected_depth_token.load(Ordering::Acquire) == 0 {
        return;
    }
    let _timer = backend.buffer_copy_timing.start();
    if !destination.is_null()
        && let Err(error) = observe_copy_buffer_resource(
            backend,
            destination,
            (!crate::kcd2_live_pose::enabled()).then_some(D3D12_RANGE { Begin: 0, End: 0 }),
        )
    {
        backend.block(error);
    }
    if !source.is_null()
        && let Err(error) = observe_copy_buffer_resource(
            backend,
            source,
            (!crate::kcd2_live_pose::enabled()).then_some(D3D12_RANGE { Begin: 0, End: 0 }),
        )
    {
        backend.block(error);
    }
    if !crate::kcd2_live_pose::enabled()
        && !destination.is_null()
        && !source.is_null()
        && bytes > 0
        && let Ok(mut tracker) = backend.render_camera.lock()
    {
        tracker.record_copy_buffer_region(
            this as usize,
            destination as usize,
            destination_offset,
            source as usize,
            source_offset,
            bytes,
        );
    }
}

fn readable_mapped_range(address: usize, bytes: usize) -> bool {
    query_readable_mapped_range(address, bytes).is_some()
}

fn query_readable_mapped_range(address: usize, bytes: usize) -> Option<MEMORY_BASIC_INFORMATION> {
    let request_end = address.checked_add(bytes)?;
    let mut info = MEMORY_BASIC_INFORMATION::default();
    let queried = unsafe {
        VirtualQuery(
            Some(address as *const c_void),
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried != size_of::<MEMORY_BASIC_INFORMATION>()
        || info.State != MEM_COMMIT
        || info.Protect.contains(PAGE_GUARD)
        || info.Protect.contains(PAGE_NOACCESS)
    {
        return None;
    }
    let base_protection = info.Protect.0 & 0xff;
    if ![
        PAGE_READONLY.0,
        PAGE_READWRITE.0,
        PAGE_WRITECOPY.0,
        PAGE_EXECUTE_READ.0,
        PAGE_EXECUTE_READWRITE.0,
        PAGE_EXECUTE_WRITECOPY.0,
    ]
    .contains(&base_protection)
    {
        return None;
    }
    let region_start = info.BaseAddress as usize;
    region_start
        .checked_add(info.RegionSize)
        .is_some_and(|region_end| address >= region_start && request_end <= region_end)
        .then_some(info)
}

fn observe_render_camera_draw(backend: &D3d12ObservationBackend, list_raw: usize) {
    if !backend.runtime_observation_active() {
        return;
    }
    let selected_token = backend.selected_depth_token.load(Ordering::Acquire);
    let Some((draw_ordinal, pass_ordinal, pass_draw_ordinal)) = backend
        .hot_lists
        .selected_draw_ordinals(list_raw, selected_token)
    else {
        return;
    };
    // Authoring windows are explicit discontinuous observations, never a recording cadence.
    // Keep real draw ordinals above, but do not accumulate matrices while the operator uses menus.
    if !crate::raw_depth_demo::authoring_camera_interval_active() {
        return;
    }
    let kcd2_enabled = crate::kcd2_live_pose::enabled();
    let production_camera = backend.render_camera_production.load(Ordering::Acquire);
    let exact_camera = kcd2_enabled || production_camera;
    let bound_production_camera = production_camera
        && backend.bound_holstered_camera.load(Ordering::Acquire)
        && !kcd2_enabled;
    let mut production_slot = None;
    let mut production_metrics = None;
    let mut production_state_revision = None;
    if !exact_camera {
        if !backend.hot_lists.sample_discovery_camera_draw(list_raw) {
            backend
                .camera_discovery_list_budget_skipped
                .fetch_add(1, Ordering::Relaxed);
            return;
        }
        backend
            .camera_discovery_draw_sampled
            .fetch_add(1, Ordering::Relaxed);
    }
    let mut kcd2_root_parameter = None;
    if kcd2_enabled {
        let Some(root_signature) = backend.hot_lists.root_signature(list_raw) else {
            return;
        };
        let Ok(fingerprints) = backend.root_fingerprints.read() else {
            return;
        };
        let Some(actual) = fingerprints.get(&root_signature) else {
            return;
        };
        // Prove the fallback copy Draw independently of the two pose source bindings.
        if let Ok(fallback) = backend.capture_fallback_draw.lock()
            && fallback.as_ref().is_some_and(|fallback| {
                fallback.draw_ordinal == pass_draw_ordinal
                    && actual.eq_ignore_ascii_case(&fallback.root_signature_sha256)
            })
        {
            backend.hot_lists.mark_fallback_draw_proven(list_raw);
        }
        kcd2_root_parameter =
            crate::kcd2_live_pose::root_parameter_for_draw(actual, pass_ordinal, pass_draw_ordinal);
        if kcd2_root_parameter.is_none() {
            return;
        }
    } else if bound_production_camera {
        let Some((slot, admitted)) = backend.production_draw_filter_match(list_raw) else {
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            crate::cp2077_camera_diagnostics::note_loss();
            return;
        };
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        slot.diagnostic_counts
            .add(crate::cp2077_camera_diagnostics::DrawCount::Selected);
        production_slot = Some(slot);
        if crate::perf::enabled() {
            let counter = if admitted {
                &slot.production_draw_prefilter_admitted
            } else {
                &slot.production_draw_prefilter_rejected
            };
            counter.fetch_add(1, Ordering::Relaxed);
            production_metrics = Some(slot);
        }
        if !admitted {
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            if crate::cp2077_camera_diagnostics::active() {
                use crate::cp2077_camera_diagnostics::DrawCount;
                slot.diagnostic_counts.add(DrawCount::SourceRejected);
                let writes = slot
                    .pipeline_hook_available
                    .load(Ordering::Acquire)
                    .then(|| {
                        backend
                            .pso_registry
                            .diagnostic_depth_write(slot.pipeline_state.load(Ordering::Acquire))
                    })
                    .flatten();
                slot.diagnostic_counts.add(match writes {
                    Some(true) => DrawCount::SourceRejectedWriting,
                    Some(false) => DrawCount::SourceRejectedNonwriting,
                    None => DrawCount::SourceRejectedUnknownPso,
                });
            }
            // CP2077 records thousands of selected-DSV draws from several CPU threads. Rejecting
            // unrelated root identities here keeps them off the process-wide camera tracker
            // mutex; admitted draws still repeat this exact-source check under the lock below.
            return;
        }
        let redundant = {
            // This predicate returns BEFORE selected_draw timing. Cover its whole
            // cost only when A3 is on: two clocks, no new per-lookup clocks/atomics.
            // Includes PSO/state checks, so it is NOT pure descriptor lock-wait time.
            let _check =
                crate::hook_timing::slow_scope(crate::perf::SlowHookKind::DrawRedundancyCheck);
            backend.hot_lists.production_draw_is_redundant(
                slot,
                &backend.cbv_descriptor_store,
                &backend.pso_registry,
            )
        };
        if redundant {
            #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
            if crate::cp2077_camera_diagnostics::active() {
                slot.diagnostic_counts
                    .add(crate::cp2077_camera_diagnostics::DrawCount::Redundant);
                if let Ok(weight) = slot.diagnostic_weight.try_lock()
                    && let Some((generation, revision, weight)) = weight.as_ref()
                    && *generation == slot.generation.load(Ordering::Acquire)
                    && *revision == slot.production_cached_revision.load(Ordering::Acquire)
                    && *revision == slot.production_state_revision.load(Ordering::Acquire)
                {
                    weight.increment();
                } else {
                    crate::cp2077_camera_diagnostics::note_loss();
                }
            }
            if crate::perf::enabled() {
                slot.production_draw_redundant
                    .fetch_add(1, Ordering::Relaxed);
            }
            return;
        }
        production_state_revision = Some(slot.production_state_revision.load(Ordering::Acquire));
    }
    let _perf = crate::perf::scope(crate::perf::Section::Draw);
    let _draw_timer = backend
        .camera_selected_draw_timing
        .start_traced(crate::perf::SlowHookKind::SelectedDraw);
    let Some(mut bindings) = backend.hot_lists.selected_draw_bindings(
        list_raw,
        draw_ordinal,
        pass_ordinal,
        pass_draw_ordinal,
    ) else {
        return;
    };
    if let Some(root_parameter) = kcd2_root_parameter {
        bindings.roots.retain(|root| {
            root.source == RootBindingSource::Direct && root.root_parameter_index == root_parameter
        });
        if let Some(root) = bindings.roots.first() {
            let candidate = backend.render_camera.lock().ok().and_then(|mut tracker| {
                tracker.direct_root_mapping_candidate(root_parameter, root.value)
            });
            if let Some((raw, offset)) = candidate {
                let range = usize::try_from(offset).ok().and_then(|begin| {
                    begin
                        .checked_add(crate::d3d12_render_camera::ROOT_CBV_SNAPSHOT_BYTES)
                        .map(|end| D3D12_RANGE {
                            Begin: begin,
                            End: end,
                        })
                });
                if let Some(range) = range
                    && let Err(error) =
                        observe_copy_buffer_resource(backend, raw as *mut c_void, Some(range))
                {
                    backend.block(format!("exact Direct root-CBV mapping failed: {error}"));
                    return;
                }
            }
        }
    }
    // Keep the extra attribution clocks off normal/other-game capture paths.
    let raw_timing = cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
        && crate::raw_depth_demo::active();
    let detailed_timing = raw_timing || production_metrics.is_some();
    let lock_timer = detailed_timing.then(|| {
        backend
            .camera_selected_draw_lock_wait_timing
            .start_traced(crate::perf::SlowHookKind::SelectedDrawLockWait)
    });
    let tracker = if kcd2_enabled {
        match backend.render_camera.lock() {
            Ok(tracker) => Some(tracker),
            Err(_) => {
                backend.note_render_camera_contention();
                None
            }
        }
    } else {
        backend.render_camera_for_selected_draw(production_metrics)
    };
    drop(lock_timer);
    let Some(mut tracker) = tracker else {
        return;
    };
    let _locked_timer = detailed_timing.then(|| {
        backend
            .camera_selected_draw_locked_timing
            .start_traced(crate::perf::SlowHookKind::SelectedDrawLocked)
    });
    if crate::raw_depth_demo::authoring_windows_enabled()
        && (!crate::raw_depth_demo::authoring_camera_interval_active()
            || selected_token != backend.selected_depth_token.load(Ordering::Acquire))
    {
        return;
    }

    if !kcd2_enabled && !tracker.matches_production_draw(&bindings) {
        if let Some(metrics) = production_metrics {
            metrics
                .production_draw_post_lock_rejected
                .fetch_add(1, Ordering::Relaxed);
        }
        return;
    }
    let fallback = backend
        .capture_fallback_draw
        .try_lock()
        .ok()
        .and_then(|fallback| fallback.clone());
    if let Some(fallback) = fallback
        && pass_draw_ordinal == fallback.draw_ordinal
        && tracker
            .root_signature_sha256(bindings.root_signature)
            .is_some_and(|actual| actual.eq_ignore_ascii_case(&fallback.root_signature_sha256))
    {
        backend.hot_lists.mark_fallback_draw_proven(list_raw);
    }
    if kcd2_enabled
        && !crate::kcd2_live_pose::accepts_draw(
            tracker
                .root_signature_sha256(bindings.root_signature)
                .as_deref(),
            pass_ordinal,
            pass_draw_ordinal,
        )
    {
        return;
    }
    // Reject unrelated draws before spending the bounded exact-binding budget. KCD2's
    // player-root source can occur well after the first two selected-DSV draws.
    if exact_camera
        && backend
            .render_camera_draw_budget
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_err()
    {
        if let Some(metrics) = production_metrics {
            metrics
                .production_draw_budget_rejected
                .fetch_add(1, Ordering::Relaxed);
        }
        backend.note_render_camera_contention();
        return;
    }
    // Only the selected, budgeted draw looks up the independent PSO registry.
    // Unknown state is an observation, never a reason to admit/reject a draw.
    if backend.draw_pass_evidence_enabled() {
        bindings.pass_context =
            Some(tracker.draw_pass_context(list_raw, backend.hot_lists.bound_dsv_flags(list_raw)));
    }
    backend.pso_registry.observe_selected(&mut bindings);
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    if let Some(slot) = production_slot {
        slot.diagnostic_counts
            .add(crate::cp2077_camera_diagnostics::DrawCount::TrackerEntered);
    }
    let observation =
        tracker.observe_selected_draw_mapped(list_raw, &bindings, |address, bytes| {
            if !readable_mapped_range(address, bytes) {
                return None;
            }
            Some(
                (0..bytes)
                    .map(|index| unsafe { std::ptr::read_volatile((address + index) as *const u8) })
                    .collect(),
            )
        });
    if let (Some(_), Some(revision)) = (production_slot, production_state_revision) {
        backend
            .hot_lists
            .publish_production_draw_observation(list_raw, revision, observation);
        #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
        if observation.staged
            && let Some(slot) = production_slot
            && let Some(weight) = crate::cp2077_camera_diagnostics::DrawWeight::new()
        {
            tracker.record_diagnostic_draw_weight(list_raw, draw_ordinal, Arc::clone(&weight));
            if let Ok(mut target) = slot.diagnostic_weight.try_lock() {
                // A diagnostic weight may represent dedup hits only if this exact observation
                // published the current cache key. Failed/stale publication cannot steal hits.
                *target = observation
                    .production_dedup_key
                    .filter(|_| {
                        slot.production_cached_revision.load(Ordering::Acquire) == revision
                            && slot.production_state_revision.load(Ordering::Acquire) == revision
                    })
                    .map(|_| (slot.generation.load(Ordering::Acquire), revision, weight));
            } else {
                crate::cp2077_camera_diagnostics::note_loss();
            }
        }
    }
    if let Some(metrics) = production_metrics {
        metrics
            .production_draw_tracker_called
            .fetch_add(1, Ordering::Relaxed);
    }
}

unsafe extern "system" fn hook_set_graphics_root_constant_buffer_view(
    this: *mut c_void,
    root_parameter_index: u32,
    buffer_location: u64,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::SET_GRAPHICS_ROOT_CONSTANT_BUFFER_VIEW,
        HookMethod::SetGraphicsRootConstantBufferView,
    ) else {
        backend.block("SetGraphicsRootConstantBufferView original target missing");
        return;
    };
    let original: SetGraphicsRootConstantBufferViewFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, root_parameter_index, buffer_location) };
    if !backend.runtime_observation_active() {
        return;
    }
    backend
        .direct_root_cbv_calls
        .fetch_add(1, Ordering::Relaxed);
    backend.hot_lists.bind_root(
        this as usize,
        root_parameter_index,
        ROOT_DIRECT_CBV,
        buffer_location,
    );
}

unsafe extern "system" fn hook_set_descriptor_heaps(
    this: *mut c_void,
    count: u32,
    heaps: *const *mut c_void,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::SET_DESCRIPTOR_HEAPS,
        HookMethod::SetDescriptorHeaps,
    ) else {
        backend.block("SetDescriptorHeaps original target missing");
        return;
    };
    let original: SetDescriptorHeapsFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, count, heaps) };
    if !backend.runtime_observation_active() {
        return;
    }
    if count > 0 && heaps.is_null() {
        backend.hot_lists.drops.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let mut observed = [0usize; 2];
    for (index, destination) in observed
        .iter_mut()
        .enumerate()
        .take(usize::try_from(count.min(2)).unwrap_or(0))
    {
        *destination = unsafe { *heaps.add(index) } as usize;
    }
    backend.hot_lists.bind_heaps(this as usize, observed);
}

unsafe extern "system" fn hook_set_graphics_root_descriptor_table(
    this: *mut c_void,
    root_parameter_index: u32,
    base_descriptor: D3D12_GPU_DESCRIPTOR_HANDLE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::SET_GRAPHICS_ROOT_DESCRIPTOR_TABLE,
        HookMethod::SetGraphicsRootDescriptorTable,
    ) else {
        backend.block("SetGraphicsRootDescriptorTable original target missing");
        return;
    };
    let original: SetGraphicsRootDescriptorTableFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, root_parameter_index, base_descriptor) };
    if !backend.runtime_observation_active() {
        return;
    }
    backend
        .descriptor_table_calls
        .fetch_add(1, Ordering::Relaxed);
    backend.hot_lists.bind_root(
        this as usize,
        root_parameter_index,
        ROOT_DESCRIPTOR_TABLE,
        base_descriptor.ptr,
    );
}

unsafe extern "system" fn hook_set_graphics_root_signature(
    this: *mut c_void,
    root_signature: *mut c_void,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::SET_GRAPHICS_ROOT_SIGNATURE,
        HookMethod::SetGraphicsRootSignature,
    ) else {
        backend.block("SetGraphicsRootSignature original target missing");
        return;
    };
    let original: SetGraphicsRootSignatureFn = unsafe { std::mem::transmute(target) };
    unsafe { original(this, root_signature) };
    if !backend.runtime_observation_active() {
        return;
    }
    backend
        .hot_lists
        .bind_root_signature(this as usize, root_signature as usize);
}

unsafe extern "system" fn hook_draw_instanced(
    this: *mut c_void,
    vertex_count_per_instance: u32,
    instance_count: u32,
    start_vertex_location: u32,
    start_instance_location: u32,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::DRAW_INSTANCED,
        HookMethod::DrawInstanced,
    ) else {
        backend.block("DrawInstanced original target missing");
        return;
    };
    let original: DrawInstancedFn = unsafe { std::mem::transmute(target) };
    unsafe {
        original(
            this,
            vertex_count_per_instance,
            instance_count,
            start_vertex_location,
            start_instance_location,
        )
    };
    observe_render_camera_draw(backend, this as usize);
}

unsafe extern "system" fn hook_draw_indexed_instanced(
    this: *mut c_void,
    index_count_per_instance: u32,
    instance_count: u32,
    start_index_location: u32,
    base_vertex_location: i32,
    start_instance_location: u32,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::DRAW_INDEXED_INSTANCED,
        HookMethod::DrawIndexedInstanced,
    ) else {
        backend.block("DrawIndexedInstanced original target missing");
        return;
    };
    let original: DrawIndexedInstancedFn = unsafe { std::mem::transmute(target) };
    unsafe {
        original(
            this,
            index_count_per_instance,
            instance_count,
            start_index_location,
            base_vertex_location,
            start_instance_location,
        )
    };
    observe_render_camera_draw(backend, this as usize);
}

unsafe extern "system" fn hook_om_set_render_targets(
    this: *mut c_void,
    render_target_count: u32,
    render_targets: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    single_range: BOOL,
    depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::OM_SET_RENDER_TARGETS,
        HookMethod::OmSetRenderTargets,
    ) else {
        backend.block("OMSetRenderTargets original target missing");
        return;
    };
    let original: OmSetRenderTargetsFn = unsafe { std::mem::transmute(target) };
    if !backend.runtime_observation_active() {
        unsafe {
            original(
                this,
                render_target_count,
                render_targets,
                single_range,
                depth,
            )
        };
        return;
    }
    backend.capture_at_proven_fallback_pass_end(this);
    unsafe {
        original(
            this,
            render_target_count,
            render_targets,
            single_range,
            depth,
        )
    };
    let mut dsv_flags = None;
    let identity = if let Ok(mut state) = backend.state.lock() {
        state.om_set_render_targets_calls += 1;
        let identity = if depth.is_null() {
            None
        } else {
            state.resolve_handle(unsafe { (*depth).ptr as u64 })
        };
        if backend.draw_pass_evidence_enabled() {
            dsv_flags = identity.and_then(|identity| state.dsv_flags_for(identity));
        }
        state.record_bound_depth(this as usize, identity);
        state.record_event(
            this as usize,
            ListEvent::BindDepth(identity),
            "OMSetRenderTargets",
        );
        identity
    } else {
        None
    };
    let selected_token = backend.selected_depth_token_for(identity);
    if depth.is_null() {
        backend.hot_lists.unbind_depth(this as usize);
    } else {
        // Even an unknown/non-selected non-null DSV advances the authored pass ordinal.
        backend
            .hot_lists
            .bind_selected_depth(this as usize, selected_token);
    }
    if backend.draw_pass_evidence_enabled() {
        backend
            .hot_lists
            .set_bound_dsv_flags(this as usize, dsv_flags);
    }
}

unsafe extern "system" fn hook_clear_depth_stencil_view(
    this: *mut c_void,
    depth: D3D12_CPU_DESCRIPTOR_HANDLE,
    flags: D3D12_CLEAR_FLAGS,
    depth_value: f32,
    stencil: u8,
    rect_count: u32,
    rects: *const RECT,
) {
    let Some(backend) = backend() else {
        return;
    };
    let Some(target) = backend.original_target(
        this,
        d3d12_abi::graphics_command_list::CLEAR_DEPTH_STENCIL_VIEW,
        HookMethod::ClearDepthStencilView,
    ) else {
        backend.block("ClearDepthStencilView original target missing");
        return;
    };
    let original: ClearDepthStencilViewFn = unsafe { std::mem::transmute(target) };
    if !backend.runtime_observation_active() {
        unsafe { original(this, depth, flags, depth_value, stencil, rect_count, rects) };
        return;
    }
    let mut diagnostic_identity = None;
    if let Ok(mut state) = backend.state.lock() {
        state.clear_depth_calls += 1;
        let identity = state.resolve_handle(depth.ptr as u64);
        diagnostic_identity = identity;
        let eligibility =
            identity.map(|identity| state.capture_source_for_clear(this as usize, identity));
        let same_list_capture_eligible = eligibility.as_ref().is_some_and(Result::is_ok);
        state.record_event(
            this as usize,
            ListEvent::ClearDepth {
                identity,
                flags: flags.0,
                depth_value_bits: depth_value.to_bits(),
                stencil,
                rect_count,
                same_list_capture_eligible,
            },
            "ClearDepthStencilView",
        );
    }
    unsafe { original(this, depth, flags, depth_value, stencil, rect_count, rects) };
    if backend.depth_order_diagnostics.load(Ordering::Acquire)
        && crate::raw_depth_demo::authoring_camera_interval_active()
        && let Some(identity) = diagnostic_identity
        && let Ok(mut tracker) = backend.render_camera.lock()
        && tracker.depth_order_diagnostics_enabled()
    {
        use crate::d3d12_render_camera::depth_order::{BoundaryKind, MAX_CLEAR_RECTS};
        let retained = (rect_count as usize).min(MAX_CLEAR_RECTS);
        let rectangles = if retained > 0 && !rects.is_null() {
            unsafe { std::slice::from_raw_parts(rects, retained) }
                .iter()
                .map(|r| [r.left, r.top, r.right, r.bottom])
                .collect()
        } else {
            Vec::new()
        };
        tracker.record_depth_boundary(
            this as usize,
            identity,
            backend.hot_lists.recorded_draw_count(this as usize),
            BoundaryKind::Clear {
                flags: flags.0 as u32,
                depth_value_bits: depth_value.to_bits(),
                stencil,
                rect_count,
                rects_complete: rectangles.len() == rect_count as usize,
                rects: rectangles,
            },
        );
    }
}

fn supported_state(state: D3D12_RESOURCE_STATES) -> Option<ResourceState> {
    if state == D3D12_RESOURCE_STATE_COMMON {
        Some(ResourceState::Common)
    } else if state == D3D12_RESOURCE_STATE_DEPTH_WRITE {
        Some(ResourceState::DepthWrite)
    } else if state == D3D12_RESOURCE_STATE_COPY_SOURCE {
        Some(ResourceState::CopySource)
    } else if state == D3D12_RESOURCE_STATE_RENDER_TARGET {
        Some(ResourceState::RenderTarget)
    } else {
        None
    }
}

const fn legacy_capture_transition_admitted(
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
    subresource: u32,
    flags: D3D12_RESOURCE_BARRIER_FLAGS,
) -> bool {
    const CP2077_DEPTH_READ: i32 = 0x40;
    const CP2077_COMPOSED_DEPTH_READ: i32 = 0xe0;
    before.0 == D3D12_RESOURCE_STATE_DEPTH_WRITE.0
        && matches!(after.0, CP2077_DEPTH_READ | CP2077_COMPOSED_DEPTH_READ)
        && subresource == D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES
        && flags.0 == D3D12_RESOURCE_BARRIER_FLAG_NONE.0
}

#[cfg(test)]
mod camera_sampling_tests;
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_pass_dsv_flags_follow_exact_handles_and_do_not_survive_rebind_or_reset() {
        let mut state = ObservationState::new();
        let metadata = DsvMetadata {
            resource_dimension: 3,
            width: 1920,
            height: 1080,
            depth_or_array_size: 1,
            mip_levels: 1,
            sample_count: 1,
            sample_quality: 0,
            resource_format: 19,
            resource_layout: 0,
            resource_flags: 2,
            dsv_format: 20,
            dsv_dimension: 3,
            dsv_flags: 0,
            view_desc_explicit: true,
        };
        state.write_dsv(10, 100, Some(metadata));
        state.write_dsv(
            20,
            100,
            Some(DsvMetadata {
                dsv_flags: 1,
                ..metadata
            }),
        );
        let first = state.resolve_handle(10).unwrap();
        let second = state.resolve_handle(20).unwrap();
        assert_eq!(first.resource, second.resource);
        assert_eq!(first.descriptor_generation, second.descriptor_generation);
        assert_eq!(state.dsv_flags_for(first), Some(0));
        assert_eq!(state.dsv_flags_for(second), Some(1));
        state.write_dsv(10, 100, None);
        assert_eq!(state.dsv_flags_for(first), None);
        assert_eq!(state.dsv_flags_for(state.resolve_handle(10).unwrap()), None);
        state.release_resource(100);
        assert_eq!(state.dsv_flags_for(second), None);

        let hot = HotListRegistry::new();
        hot.register(7, 1);
        hot.bind_selected_depth(7, 1);
        hot.set_bound_dsv_flags(7, Some(0));
        assert_eq!(hot.bound_dsv_flags(7), Some(0));
        hot.bind_selected_depth(7, 2);
        assert_eq!(hot.bound_dsv_flags(7), None);
        hot.set_bound_dsv_flags(7, Some(1));
        hot.unbind_depth(7);
        assert_eq!(hot.bound_dsv_flags(7), None);
        hot.set_bound_dsv_flags(7, Some(1));
        hot.reset_with_pipeline(7, 0);
        assert_eq!(hot.bound_dsv_flags(7), None);
        hot.set_bound_dsv_flags(7, Some(1));
        hot.release(7);
        hot.register(7, 2);
        assert_eq!(hot.bound_dsv_flags(7), None);
    }

    #[test]
    fn draw_pass_scissor_forwards_original_before_observing_and_stays_gated() {
        struct Call {
            backend: Arc<D3d12ObservationBackend>,
            called: bool,
        }
        unsafe extern "system" fn original(this: *mut c_void, _count: u32, _rects: *const RECT) {
            let call = unsafe { &mut *this.cast::<Call>() };
            assert!(
                call.backend
                    .render_camera
                    .lock()
                    .unwrap()
                    .draw_pass_context(this as usize, None)
                    .scissor
                    .is_none()
            );
            call.called = true;
        }
        for (runtime, diagnostic) in [(false, false), (true, false), (false, true), (true, true)] {
            for (count, readable) in [(0, true), (1, true), (2, true), (1, false)] {
                let backend = D3d12ObservationBackend::new();
                let mut call = Call {
                    backend: Arc::clone(&backend),
                    called: false,
                };
                let raw = (&mut call as *mut Call).cast::<c_void>();
                backend
                    .render_camera
                    .lock()
                    .unwrap()
                    .register_list(raw as usize, 1);
                backend.set_runtime_observation_active(runtime);
                backend
                    .depth_order_diagnostics
                    .store(diagnostic, Ordering::Release);
                let rect = RECT {
                    left: 0,
                    top: 0,
                    right: 1920,
                    bottom: 1080,
                };
                unsafe {
                    scissor_after_original(
                        &backend,
                        original,
                        raw,
                        count,
                        if readable { &rect } else { std::ptr::null() },
                    );
                }
                assert!(call.called);
                let observed = backend
                    .render_camera
                    .lock()
                    .unwrap()
                    .draw_pass_context(raw as usize, None)
                    .scissor;
                if runtime && diagnostic {
                    let observed = observed.unwrap();
                    assert_eq!(observed.count, count);
                    assert_eq!(
                        observed.single_rect,
                        (count == 1 && readable).then_some([0, 0, 1920, 1080])
                    );
                } else {
                    assert_eq!(observed, None);
                }
            }
        }
    }
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use windows::Win32::Graphics::Direct3D12::D3D12_CLEAR_FLAG_DEPTH;

    #[test]
    fn cold_camera_descriptor_lifetime_survives_start_and_invalidates_during_idle() {
        let backend = D3d12ObservationBackend::new();
        assert!(!backend.runtime_observation_active());
        observe_camera_cbv_descriptor(&backend, 0xfc0, 0x100000, 1024);
        assert_eq!(
            backend.cbv_descriptor_store.len(),
            1,
            "load-time camera CBV must be known before StartRun"
        );
        apply_descriptor_copy(
            &backend,
            CbvDescriptorStore::prepare_copy(&[0x1000], &[0xfc0]).unwrap(),
        );
        let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
        ))
        .unwrap();
        let source = binding.view_sources[0].source.descriptor_table.clone();
        {
            let mut tracker = backend.render_camera.lock().unwrap();
            tracker.register_buffer(0x100, 0x100000, 1024);
            tracker.map_buffer(0x100, 0x200000);
            tracker.register_descriptor_heap(0x300, 0xfe0, 0x500000, 8, 32, true);
            tracker.register_root_signature_with_fingerprint(
                0x600,
                source.root_signature_sha256,
                BTreeMap::from([(
                    source.root_parameter_index,
                    vec![RootCbvRange {
                        table_offset: 0,
                        descriptor_count: source.descriptor_count,
                        descriptor_range_index: source.descriptor_range_index,
                        base_shader_register: source.base_shader_register,
                        register_space: source.register_space,
                    }],
                )]),
            );
            tracker.register_list(0x200, 9);
        }
        backend
            .configure_production_camera(
                binding,
                depth_lab::CameraPoseStreamContext {
                    identity: depth_lab::StreamIdentity {
                        session_id: "cold-descriptor-test".into(),
                        run_id: "first-run".into(),
                        generation: 1,
                    },
                    dsv_id: "selected-depth".into(),
                },
            )
            .unwrap();
        backend.set_runtime_observation_active(true);
        assert_eq!(backend.cbv_descriptor_store.len(), 2);
        {
            let mut tracker = backend.render_camera.lock().unwrap();
            tracker.arm_selected_depth(DsvIdentity {
                handle: DescriptorHandle(4),
                descriptor_generation: 1,
                resource: ResourceId(7),
                resource_epoch: 1,
            });
            tracker.observe_selected_draw(
                0x200,
                &SelectedDrawBindings {
                    pipeline_state: 0,
                    pass_context: None,
                    pipeline_depth: None,
                    list_generation: 1,
                    draw_ordinal: 1,
                    pass_ordinal: 0,
                    draw_ordinal_in_pass: 0,
                    descriptor_heaps: [0x300, 0],
                    root_signature: 0x600,
                    roots: vec![ObservedRootBinding {
                        root_parameter_index: source.root_parameter_index,
                        source: RootBindingSource::DescriptorTableBase,
                        value: 0x500000,
                    }],
                },
                |_, _, _| None,
            );
            let snapshot = tracker.snapshot();
            assert_eq!(snapshot.selected_descriptor_table_draws, 1);
            assert_eq!(snapshot.selected_table_resolved_descriptors, 1);
            assert!(
                snapshot.observations.is_empty(),
                "identity alone cannot emit a pose"
            );
        }
        backend.set_runtime_observation_active(false);
        invalidate_camera_descriptor(&backend, 0xfc0);
        apply_descriptor_copy(
            &backend,
            CbvDescriptorStore::prepare_copy(&[0x1000], &[0xfc0]).unwrap(),
        );
        assert_eq!(backend.cbv_descriptor_store.len(), 0);
        observe_camera_cbv_descriptor(&backend, 0xfc0, 0x200000, 1024);
        assert_eq!(backend.cbv_descriptor_store.len(), 1);
        assert!(!backend.runtime_observation_active());
    }

    #[test]
    fn cp2077_catalog_depth_plane_accepts_observed_whole_resource_transition() {
        let profile: depth_lab::DepthProfile = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/depth-profile.json"
        ))
        .unwrap();
        let selection = profile.d3d12_selection.unwrap();
        selection.validate().unwrap();
        let backend = D3d12ObservationBackend::new();
        let transition = selection.transition;
        backend
            .capture_transition_before
            .store(transition.state_before, Ordering::Relaxed);
        backend
            .capture_transition_after
            .store(transition.state_after, Ordering::Relaxed);
        backend
            .capture_transition_subresource
            .store(transition.subresource, Ordering::Relaxed);
        backend
            .capture_transition_flags
            .store(transition.barrier_flags, Ordering::Relaxed);
        backend
            .capture_exact_transition_enabled
            .store(true, Ordering::Release);
        // The failed live warmup observed this all-subresources 16 -> 64 transition 207 times.
        for subresource in [0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES] {
            assert!(backend.capture_transition_admitted(
                D3D12_RESOURCE_STATES(16),
                D3D12_RESOURCE_STATES(64),
                subresource,
                D3D12_RESOURCE_BARRIER_FLAG_NONE,
            ));
        }
        for (before, after, subresource, flags) in [
            (16, 64, 1, D3D12_RESOURCE_BARRIER_FLAG_NONE),
            (
                16,
                224,
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_BARRIER_FLAG_NONE,
            ),
            (
                64,
                16,
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_BARRIER_FLAG_NONE,
            ),
            (
                16,
                64,
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY,
            ),
        ] {
            assert!(!backend.capture_transition_admitted(
                D3D12_RESOURCE_STATES(before),
                D3D12_RESOURCE_STATES(after),
                subresource,
                flags,
            ));
        }
    }

    #[test]
    fn hot_descriptor_operations_complete_while_render_camera_is_locked() {
        let backend = D3d12ObservationBackend::new();
        let held = backend.render_camera.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_backend
                .cbv_descriptor_store
                .write_cbv_descriptor(0xfc0, 0x100000, 256);
            let plan = CbvDescriptorStore::prepare_copy(&[0x1000], &[0xfc0]).unwrap();
            apply_descriptor_copy(&worker_backend, plan);
            worker_backend
                .cbv_descriptor_store
                .invalidate_cbv_descriptor(0xfc0);
            let plan = CbvDescriptorStore::prepare_copy(&[0x4020], &[0x8020]).unwrap();
            apply_descriptor_copy(&worker_backend, plan);
            tx.send(worker_backend.cbv_descriptor_store.len()).unwrap();
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(5));
        drop(held);
        assert_eq!(
            result.expect("hot descriptor path waited for the main lock"),
            1
        );
        worker.join().unwrap();
        assert_eq!(backend.render_camera_snapshot().tracked_cbv_descriptors, 1);
        assert_eq!(backend.descriptor_copy_untracked.load(Ordering::Relaxed), 1);
        assert_eq!(backend.descriptor_copy_lock_wait_timing.snapshot().calls, 2);
        assert_eq!(backend.descriptor_copy_apply_timing.snapshot().calls, 2);
    }

    static FAKE_QUEUE: AtomicUsize = AtomicUsize::new(0);
    static FAKE_LIST: AtomicUsize = AtomicUsize::new(0);
    static FORWARDED_CALLS: AtomicU64 = AtomicU64::new(0);

    /// The histogram only earns its place if the bucket edges are exact; an
    /// off-by-one at 8 or 128 would misreport exactly the shape it exists to
    /// show.
    #[test]
    fn descriptor_copy_batch_buckets_split_on_their_stated_edges() {
        let backend = D3d12ObservationBackend::new();
        // Each pair straddles one stated edge; 0 and 1 share the first bucket.
        let batches = [0u64, 1, 2, 8, 9, 32, 33, 128, 129, 512, 513, 4096];
        for descriptors in batches {
            note_descriptor_copy_batch(&backend, descriptors);
        }
        let counts: Vec<u64> = backend
            .descriptor_copy_batch_buckets
            .iter()
            .map(|bucket| bucket.load(Ordering::Relaxed))
            .collect();
        assert_eq!(counts, vec![2, 2, 2, 2, 2, 2]);
        assert_eq!(
            backend.descriptor_copy_descriptors.load(Ordering::Relaxed),
            batches.iter().sum::<u64>()
        );
    }

    #[test]
    fn descriptor_copy_shard_buckets_count_the_union_once_and_keep_call_timing() {
        let backend = D3d12ObservationBackend::new();
        for count in [0, 1, 2, 3, 4, 5, 8, 9, 64] {
            let handles: Vec<_> = (0..count).map(|i| 0x100000 + i * 4096).collect();
            let plan = CbvDescriptorStore::prepare_copy(&handles, &handles).unwrap();
            assert_eq!(plan.shard_count, count as u32);
            apply_descriptor_copy(&backend, plan);
        }
        let snapshot = backend.render_camera_snapshot();
        for (name, count) in [("1", 1), ("2", 1), ("3_4", 2), ("5_8", 2), ("9_plus", 2)] {
            let value = &snapshot.hook_cpu_timing[&format!("descriptor_copy_shards_{name}")];
            assert_eq!(value.calls, count);
            assert_eq!((value.total_us, value.max_us), (0, 0));
        }
        assert_eq!(
            snapshot.hook_cpu_timing["descriptor_copy_lock_wait"].calls,
            9
        );
        assert_eq!(snapshot.hook_cpu_timing["descriptor_copy_apply"].calls, 9);
    }

    #[test]
    fn runtime_hot_observation_is_explicit_and_off_before_start_run() {
        let backend = D3d12ObservationBackend::new();
        assert!(!backend.runtime_observation_active());
        backend.set_runtime_observation_active(true);
        assert!(backend.runtime_observation_active());
        backend.set_runtime_observation_active(false);
        assert!(!backend.runtime_observation_active());
    }

    #[test]
    fn production_draw_metrics_are_list_local_and_survive_recording_reset() {
        let backend = D3d12ObservationBackend::new();
        backend.hot_lists.register(0x2000, 1);
        backend.hot_lists.register(0x3000, 2);
        let first = backend.hot_lists.find(0x2000).unwrap();
        let second = backend.hot_lists.find(0x3000).unwrap();
        first
            .production_draw_prefilter_admitted
            .fetch_add(3, Ordering::Relaxed);
        first
            .production_draw_lock_contended
            .fetch_add(2, Ordering::Relaxed);
        second
            .production_draw_prefilter_rejected
            .fetch_add(5, Ordering::Relaxed);
        second
            .production_draw_tracker_called
            .fetch_add(1, Ordering::Relaxed);
        backend.hot_lists.reset(0x2000);
        backend.hot_lists.release(0x3000);

        let snapshot = backend.hook_timing_snapshot();
        assert_eq!(snapshot["production_draw_prefilter_admitted"].calls, 3);
        assert_eq!(snapshot["production_draw_prefilter_rejected"].calls, 5);
        assert_eq!(snapshot["production_draw_lock_contended"].calls, 2);
        assert_eq!(snapshot["production_draw_tracker_called"].calls, 1);
    }

    #[test]
    fn production_draw_redundancy_requires_exact_descriptor_and_unchanged_list_state() {
        let backend = D3d12ObservationBackend::new();
        let list = 0x2000;
        let handle = 0x10_0000;
        backend.hot_lists.register(list, 1);
        backend.hot_lists.set_pipeline_hook_available(list, true);
        backend
            .hot_lists
            .bind_root(list, 18, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        backend
            .cbv_descriptor_store
            .write_cbv_descriptor(handle, 0x20_0000, 256);
        let generation = backend.cbv_descriptor_store.generation(handle).unwrap();
        let observation = |generation| SelectedDrawObservation {
            staged: true,
            production_dedup_key: Some(ProductionDrawDedupKey {
                descriptor_identities: [(handle, generation), (0, 0), (0, 0), (0, 0)],
                descriptor_count: 1,
                root_bindings: [(18, 0x50_0000), (0, 0), (0, 0), (0, 0)],
                root_binding_count: 1,
            }),
        };
        let revision = backend
            .hot_lists
            .find(list)
            .unwrap()
            .production_state_revision
            .load(Ordering::Acquire);
        backend.hot_lists.publish_production_draw_observation(
            list,
            revision,
            observation(generation),
        );
        let slot = backend.hot_lists.find(list).unwrap();
        assert!(backend.hot_lists.production_draw_is_redundant(
            slot,
            &backend.cbv_descriptor_store,
            &backend.pso_registry,
        ));
        assert!(backend.hot_lists.has_production_camera_work(&[list]));

        backend.hot_lists.set_pipeline_hook_available(list, false);
        assert!(
            !backend.hot_lists.production_draw_is_redundant(
                slot,
                &backend.cbv_descriptor_store,
                &backend.pso_registry,
            ),
            "missing SetPipelineState coverage must disable Draw redundancy"
        );
        backend.hot_lists.set_pipeline_hook_available(list, true);
        let revision = slot.production_state_revision.load(Ordering::Acquire);
        backend.hot_lists.publish_production_draw_observation(
            list,
            revision,
            observation(generation),
        );

        // Re-authoring the same descriptor creates a new exact generation even when its bytes and
        // GPU VA happen to match. The old representative must not survive it.
        backend
            .cbv_descriptor_store
            .write_cbv_descriptor(handle, 0x20_0000, 256);
        let next_generation = backend.cbv_descriptor_store.generation(handle).unwrap();
        assert_ne!(generation, next_generation);
        assert!(!backend.hot_lists.production_draw_is_redundant(
            slot,
            &backend.cbv_descriptor_store,
            &backend.pso_registry,
        ));

        backend
            .hot_lists
            .bind_root(list, 18, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        let revision = slot.production_state_revision.load(Ordering::Acquire);
        backend.hot_lists.publish_production_draw_observation(
            list,
            revision,
            observation(next_generation),
        );
        assert!(backend.hot_lists.production_draw_is_redundant(
            slot,
            &backend.cbv_descriptor_store,
            &backend.pso_registry,
        ));
        backend.hot_lists.bind_pipeline_state(list, 0x7000);
        assert!(!backend.hot_lists.production_draw_is_redundant(
            slot,
            &backend.cbv_descriptor_store,
            &backend.pso_registry,
        ));
        // Earlier staged work remains required even though a later state change invalidated only
        // the Draw representative.
        assert!(backend.hot_lists.has_production_camera_work(&[list]));

        backend.pso_registry.register_test_writer(0x7000);
        backend.pso_registry.register_test_writer(0x8000);
        let revision = slot.production_state_revision.load(Ordering::Acquire);
        backend.hot_lists.publish_production_draw_observation(
            list,
            revision,
            observation(next_generation),
        );
        backend.hot_lists.bind_pipeline_state(list, 0x8000);
        assert!(
            backend.hot_lists.production_draw_is_redundant(
                slot,
                &backend.cbv_descriptor_store,
                &backend.pso_registry,
            ),
            "different PSOs with identical proven depth/stencil state must preserve the representative"
        );
        backend
            .hot_lists
            .bind_root(list, 5, ROOT_DESCRIPTOR_TABLE, 0x70_0000);
        assert!(
            backend.hot_lists.production_draw_is_redundant(
                slot,
                &backend.cbv_descriptor_store,
                &backend.pso_registry,
            ),
            "unrelated material roots must not invalidate the camera representative"
        );
        backend
            .hot_lists
            .bind_root(list, 18, ROOT_DESCRIPTOR_TABLE, 0x60_0000);
        assert!(!backend.hot_lists.production_draw_is_redundant(
            slot,
            &backend.cbv_descriptor_store,
            &backend.pso_registry,
        ));

        backend
            .hot_lists
            .bind_root(list, 18, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        let revision = slot.production_state_revision.load(Ordering::Acquire);
        backend.hot_lists.publish_production_draw_observation(
            list,
            revision,
            observation(next_generation),
        );
        backend.hot_lists.mark_production_depth_copy_recorded(list);
        assert!(!backend.hot_lists.production_draw_is_redundant(
            slot,
            &backend.cbv_descriptor_store,
            &backend.pso_registry,
        ));
        backend.hot_lists.reset(list);
        assert!(!backend.hot_lists.has_production_camera_work(&[list]));
        assert_eq!(
            slot.production_cached_descriptor_count
                .load(Ordering::Acquire),
            0
        );
    }

    #[test]
    fn bound_production_activation_marks_existing_lists_for_one_conservative_execute() {
        let backend = D3d12ObservationBackend::new();
        let list = 0x2000;
        backend.hot_lists.register(list, 1);
        assert!(!backend.hot_lists.has_production_camera_work(&[list]));
        let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
        ))
        .unwrap();

        backend
            .configure_production_camera(
                binding,
                depth_lab::CameraPoseStreamContext {
                    identity: depth_lab::StreamIdentity {
                        session_id: "activation-race-test".into(),
                        run_id: "first-run".into(),
                        generation: 1,
                    },
                    dsv_id: "selected-depth".into(),
                },
            )
            .unwrap();

        assert!(backend.hot_lists.has_production_camera_work(&[list]));
        backend.hot_lists.clear_production_camera_work(&[list]);
        assert!(!backend.hot_lists.has_production_camera_work(&[list]));
    }

    #[test]
    fn production_draw_prefilter_does_not_take_the_render_camera_mutex() {
        let backend = D3d12ObservationBackend::new();
        let binding: depth_lab::D3d12CameraBinding = serde_json::from_str(include_str!(
            "../../../config/catalogs/dev/cyberpunk-2077-steam/camera-binding.json"
        ))
        .unwrap();
        let expected_sha256 = binding.view_sources[0]
            .source
            .descriptor_table
            .root_signature_sha256
            .clone();
        backend
            .configure_production_camera(
                binding,
                depth_lab::CameraPoseStreamContext {
                    identity: depth_lab::StreamIdentity {
                        session_id: "prefilter-test".into(),
                        run_id: "first-run".into(),
                        generation: 1,
                    },
                    dsv_id: "selected-depth".into(),
                },
            )
            .unwrap();
        backend.hot_lists.register(0x2000, 1);
        backend.hot_lists.bind_root_signature(0x2000, 0x6000);
        backend
            .hot_lists
            .bind_root(0x2000, 18, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        backend
            .root_fingerprints
            .write()
            .unwrap()
            .insert(0x6000, expected_sha256);

        let held = backend.render_camera.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            result_tx
                .send(worker_backend.matches_production_draw_filter(0x2000))
                .unwrap();
        });
        let matched = result_rx.recv_timeout(std::time::Duration::from_secs(1));
        drop(held);
        worker.join().unwrap();
        assert!(
            matched.unwrap(),
            "authored aliases must be filtered without waiting for the camera tracker"
        );

        backend
            .hot_lists
            .bind_root(0x2000, 18, ROOT_DESCRIPTOR_TABLE, 0);
        backend
            .hot_lists
            .bind_root(0x2000, 0, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        assert!(backend.matches_production_draw_filter(0x2000));

        backend
            .hot_lists
            .bind_root(0x2000, 18, ROOT_DIRECT_CBV, 0x50_0000);
        backend
            .hot_lists
            .bind_root(0x2000, 0, ROOT_DESCRIPTOR_TABLE, 0);
        assert!(!backend.matches_production_draw_filter(0x2000));
        backend
            .root_fingerprints
            .write()
            .unwrap()
            .insert(0x6000, format!("sha256:{}", "0".repeat(64)));
        backend
            .hot_lists
            .bind_root(0x2000, 18, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        assert!(!backend.matches_production_draw_filter(0x2000));
    }

    #[test]
    fn discovery_execute_preserves_prepared_work_during_tracker_contention() {
        let backend = D3d12ObservationBackend::new();
        let held = backend.render_camera.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(worker_backend.render_camera_after_execute().is_ok())
                .unwrap();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let while_held = result_rx.recv_timeout(std::time::Duration::from_millis(50));
        drop(held);
        let committed = while_held.as_ref().copied().unwrap_or_else(|_| {
            result_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
        });
        worker.join().unwrap();
        assert!(
            while_held.is_err(),
            "local Execute must not silently discard its prepared camera work on WouldBlock"
        );
        assert!(committed);
    }

    #[test]
    fn production_execute_retains_would_block_for_existing_fail_closed_policy() {
        let backend = D3d12ObservationBackend::new();
        backend
            .render_camera_production
            .store(true, Ordering::Release);
        let held = backend.render_camera.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            result_tx
                .send(matches!(
                    worker_backend.render_camera_after_execute(),
                    Err(std::sync::TryLockError::WouldBlock)
                ))
                .unwrap();
        });
        let result = result_rx.recv_timeout(std::time::Duration::from_secs(1));
        drop(held);
        worker.join().unwrap();
        assert!(result.unwrap());
    }

    #[test]
    fn bound_camera_captures_pass_state_without_enabling_unbounded_run_diagnostics() {
        let backend = D3d12ObservationBackend::new();
        assert!(!backend.draw_pass_evidence_enabled());
        backend
            .bound_holstered_camera
            .store(true, Ordering::Release);
        assert!(backend.draw_pass_evidence_enabled());
        assert!(!backend.depth_order_diagnostics.load(Ordering::Acquire));
        assert!(
            !backend
                .render_camera
                .lock()
                .unwrap()
                .depth_order_diagnostics_enabled()
        );
        backend.disarm_selected_depth_hot_path();
        assert!(!backend.draw_pass_evidence_enabled());
    }

    #[test]
    fn discovery_selected_draw_survives_tracker_contention_without_erasing_prior_loss() {
        let backend = D3d12ObservationBackend::new();
        backend
            .render_camera_contention_drops
            .store(3, Ordering::Relaxed);
        let held = backend.render_camera.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx
                .send(
                    worker_backend
                        .render_camera_for_selected_draw(None)
                        .is_some(),
                )
                .unwrap();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let while_held = result_rx.recv_timeout(std::time::Duration::from_millis(50));
        drop(held);
        let acquired = while_held.as_ref().copied().unwrap_or_else(|_| {
            result_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
        });
        worker.join().unwrap();
        assert!(
            while_held.is_err(),
            "discovery must preserve the selected draw until bookkeeping releases the tracker"
        );
        assert!(acquired);
        assert_eq!(
            backend
                .render_camera_contention_drops
                .load(Ordering::Relaxed),
            3
        );
    }

    #[test]
    fn bound_holstered_camera_preserves_draw_execute_and_present_during_contention() {
        for operation in ["draw", "execute", "present"] {
            let backend = D3d12ObservationBackend::new();
            backend
                .render_camera_production
                .store(true, Ordering::Release);
            backend
                .bound_holstered_camera
                .store(true, Ordering::Release);
            let held = backend.render_camera.lock().unwrap();
            let worker_backend = Arc::clone(&backend);
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                started_tx.send(()).unwrap();
                let kept = match operation {
                    "draw" => worker_backend
                        .render_camera_for_selected_draw(None)
                        .is_some(),
                    "execute" => worker_backend.render_camera_after_execute().is_ok(),
                    "present" => worker_backend.take_production_camera(1).is_ok(),
                    _ => unreachable!(),
                };
                result_tx.send(kept).unwrap();
            });
            started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap();
            let while_held = result_rx.recv_timeout(std::time::Duration::from_millis(50));
            drop(held);
            let kept = while_held.as_ref().copied().unwrap_or_else(|_| {
                result_rx
                    .recv_timeout(std::time::Duration::from_secs(1))
                    .unwrap()
            });
            worker.join().unwrap();
            assert!(
                while_held.is_err(),
                "bound camera {operation} must preserve work while bookkeeping owns the tracker"
            );
            assert!(kept, "bound camera {operation} discarded work");
            assert_eq!(
                backend
                    .render_camera_contention_drops
                    .load(Ordering::Acquire),
                0
            );
        }
    }

    #[test]
    fn production_selected_draw_keeps_nonblocking_contention_rejection() {
        let backend = D3d12ObservationBackend::new();
        backend
            .render_camera_production
            .store(true, Ordering::Release);
        let held = backend.render_camera.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            result_tx
                .send(
                    worker_backend
                        .render_camera_for_selected_draw(None)
                        .is_some(),
                )
                .unwrap();
        });
        let result = result_rx.recv_timeout(std::time::Duration::from_secs(1));
        drop(held);
        worker.join().unwrap();
        assert!(!result.unwrap());
        assert_eq!(
            backend
                .render_camera_contention_drops
                .load(Ordering::Relaxed),
            1
        );
    }

    #[test]
    fn discovery_selected_draw_rejects_poisoned_tracker() {
        let backend = D3d12ObservationBackend::new();
        let worker_backend = Arc::clone(&backend);
        assert!(
            std::thread::spawn(move || {
                let _held = worker_backend.render_camera.lock().unwrap();
                panic!("test tracker poison");
            })
            .join()
            .is_err()
        );
        assert!(backend.render_camera_for_selected_draw(None).is_none());
        assert_eq!(
            backend
                .render_camera_contention_drops
                .load(Ordering::Relaxed),
            1
        );
        let snapshot = backend.render_camera_snapshot();
        assert!(!snapshot.candidate_admitted_for_cross_launch_discovery);
        assert!(
            snapshot
                .authoring_blockers
                .iter()
                .any(|value| value.contains("tracker_contention_drops=1"))
        );
    }

    #[test]
    fn snapshot_releases_observation_state_before_waiting_for_swapchains() {
        let backend = D3d12ObservationBackend::new();
        let swapchains = backend.swapchains.lock().unwrap();
        let worker_backend = Arc::clone(&backend);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            worker_backend.snapshot()
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let observation_state_is_available = backend.state.try_lock().is_ok();
        drop(swapchains);
        worker.join().unwrap();
        assert!(observation_state_is_available);
    }

    #[test]
    fn capture_transition_is_only_the_exact_cp2077_full_resource_depth_write_exit() {
        assert!(legacy_capture_transition_admitted(
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATES(0xe0),
            D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            D3D12_RESOURCE_BARRIER_FLAG_NONE,
        ));
        assert!(legacy_capture_transition_admitted(
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATES(0x40),
            D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            D3D12_RESOURCE_BARRIER_FLAG_NONE,
        ));
        for (before, after, subresource, flags) in [
            (
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATES(0xe0),
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_BARRIER_FLAG_NONE,
            ),
            (
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_BARRIER_FLAG_NONE,
            ),
            (
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
                D3D12_RESOURCE_STATES(0xe0),
                0,
                D3D12_RESOURCE_BARRIER_FLAG_NONE,
            ),
            (
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
                D3D12_RESOURCE_STATES(0xe0),
                D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY,
            ),
        ] {
            assert!(!legacy_capture_transition_admitted(
                before,
                after,
                subresource,
                flags
            ));
        }
    }

    #[test]
    fn stable_profile_transition_replaces_the_cp2077_legacy_gate() {
        let backend = D3d12ObservationBackend::new();
        backend
            .capture_transition_before
            .store(D3D12_RESOURCE_STATE_DEPTH_WRITE.0, Ordering::Relaxed);
        backend
            .capture_transition_after
            .store(D3D12_RESOURCE_STATE_COPY_SOURCE.0, Ordering::Relaxed);
        backend
            .capture_transition_subresource
            .store(0, Ordering::Relaxed);
        backend
            .capture_transition_flags
            .store(D3D12_RESOURCE_BARRIER_FLAG_NONE.0, Ordering::Relaxed);
        backend
            .capture_exact_transition_enabled
            .store(true, Ordering::Release);

        assert!(backend.capture_transition_admitted(
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            0,
            D3D12_RESOURCE_BARRIER_FLAG_NONE,
        ));
        assert!(!backend.capture_transition_admitted(
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
            D3D12_RESOURCE_STATES(0xe0),
            D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            D3D12_RESOURCE_BARRIER_FLAG_NONE,
        ));
    }

    #[test]
    fn kcd2_depth_plane_barriers_prove_two_clear_intervals_independently_of_stencil() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(
            0x3000,
            0x4000,
            Some(DsvMetadata {
                resource_dimension: 3,
                width: 1920,
                height: 1080,
                depth_or_array_size: 1,
                mip_levels: 1,
                sample_count: 1,
                sample_quality: 0,
                resource_format: DXGI_FORMAT_R24G8_TYPELESS.0,
                resource_layout: 0,
                resource_flags: 2,
                dsv_format: DXGI_FORMAT_D24_UNORM_S8_UINT.0,
                dsv_dimension: 3,
                dsv_flags: 0,
                view_desc_explicit: true,
            }),
        );
        let identity = state.resolve_handle(0x3000).unwrap();
        let transition = |before, after, subresource, split| RawTransition {
            resource: 0x4000,
            before: D3D12_RESOURCE_STATES(before),
            after: D3D12_RESOURCE_STATES(after),
            subresource,
            split,
        };
        for _ in 0..2 {
            state.reset_list(0x2000);
            // Stencil DEPTH_WRITE alone cannot prove depth is writable.
            state.record_transition_state(0x2000, transition(128, 16, 1, false));
            assert!(state.capture_source_for_clear(0x2000, identity).is_err());
            for barrier in [transition(128, 16, 0, false), transition(16, 128, 1, false)] {
                state.record_transition_state(0x2000, barrier);
                state.record_event(0x2000, ListEvent::Transition(barrier), "barrier");
            }
            assert!(state.capture_source_for_clear(0x2000, identity).is_ok());
            state.record_event(
                0x2000,
                ListEvent::ClearDepth {
                    identity: Some(identity),
                    flags: D3D12_CLEAR_FLAG_DEPTH.0,
                    depth_value_bits: 0.0f32.to_bits(),
                    stencil: 0,
                    rect_count: 0,
                    same_list_capture_eligible: true,
                },
                "clear",
            );
            let barrier = transition(16, 128, 0, false);
            state.record_transition_state(0x2000, barrier);
            state.record_event(0x2000, ListEvent::Transition(barrier), "barrier");
            assert!(state.capture_source_for_clear(0x2000, identity).is_err());
            state.execute(0x1000, &[0x2000usize as *mut c_void]);
            state.note_present();
        }
        assert!(state.blockers.is_empty(), "{:?}", state.blockers);
        let candidate = &state.candidate_observations()[0];
        assert_eq!(candidate.eligible_clear_calls, 2);
        assert_eq!(candidate.present_intervals_with_clear, 2);

        state.record_transition_state(0x2000, transition(128, 16, 0, false));
        assert!(state.capture_source_for_clear(0x2000, identity).is_ok());
        state.record_transition_state(0x2000, transition(16, 128, 0, true));
        assert!(state.capture_source_for_clear(0x2000, identity).is_err());
        state
            .resources
            .get_mut(&0x4000)
            .unwrap()
            .single_mip_depth_plane = false;
        state.record_transition_state(0x2000, transition(128, 16, 0, false));
        state.apply_transition(1, transition(128, 16, 0, false));
        assert!(state.capture_source_for_clear(0x2000, identity).is_err());
        assert!(
            state
                .blockers
                .iter()
                .any(|error| error.contains("per-subresource"))
        );
    }

    #[test]
    fn derived_graphics_list_interface_keeps_verified_base_hook_layout() {
        use windows::Win32::Graphics::Direct3D12::ID3D12GraphicsCommandList1;
        let (device, _) = crate::d3d12_candidate::tests::warp_device();
        let allocator: windows::Win32::Graphics::Direct3D12::ID3D12CommandAllocator =
            unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }.unwrap();
        let list: ID3D12GraphicsCommandList1 = unsafe {
            device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None::<&windows::Win32::Graphics::Direct3D12::ID3D12PipelineState>,
            )
        }
        .unwrap();
        assert!(
            unsafe { validate_graphics_list_base(list.as_raw(), &ID3D12GraphicsCommandList1::IID) }
                .is_ok()
        );
        assert!(
            unsafe {
                validate_graphics_list_base(std::ptr::null_mut(), &ID3D12GraphicsCommandList1::IID)
            }
            .is_err()
        );
        unsafe { list.Close() }.unwrap();
    }

    fn armed_test_capture() -> std::sync::Arc<D3d12ObservationBackend> {
        armed_test_capture_with_storage(false).0
    }

    fn armed_test_capture_with_storage(
        gpu_only: bool,
    ) -> (
        std::sync::Arc<D3d12ObservationBackend>,
        ID3D12Device,
        ID3D12CommandQueue,
        ID3D12Resource,
    ) {
        use windows::Win32::Graphics::Direct3D12::*;
        let (device, queue) = crate::d3d12_candidate::tests::warp_device();
        let mut source = None;
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    CreationNodeMask: 1,
                    VisibleNodeMask: 1,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 2,
                    Height: 2,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: DXGI_FORMAT_D32_FLOAT,
                    SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
                None,
                &mut source,
            )
        }
        .unwrap();
        let source: ID3D12Resource = source.unwrap();
        let backend = D3d12ObservationBackend::new();
        backend.state.lock().unwrap().write_dsv(
            0x3000,
            source.as_raw() as usize,
            Some(DsvMetadata {
                resource_dimension: 3,
                width: 2,
                height: 2,
                depth_or_array_size: 1,
                mip_levels: 1,
                sample_count: 1,
                sample_quality: 0,
                resource_format: DXGI_FORMAT_D32_FLOAT.0,
                resource_layout: 0,
                resource_flags: 2,
                dsv_format: DXGI_FORMAT_D32_FLOAT.0,
                dsv_dimension: 3,
                dsv_flags: 0,
                view_desc_explicit: true,
            }),
        );
        backend
            .arm_depth_capture_storage(0x3000, 4, gpu_only, None, None)
            .unwrap();
        (backend, device, queue, source)
    }

    #[test]
    fn stop_closes_open_interval_before_late_depth_copy_can_enter() {
        use windows::Win32::Graphics::Direct3D12::*;
        let (backend, device, _, source) = armed_test_capture_with_storage(true);
        let allocator: ID3D12CommandAllocator =
            unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }.unwrap();
        let list: ID3D12GraphicsCommandList = unsafe {
            device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None::<&ID3D12PipelineState>,
            )
        }
        .unwrap();
        backend.begin_depth_capture_interval(653, true);
        // StopRun observes zero work while the next render interval is still open.
        assert_eq!(backend.capture_slots_in_flight(), 0);
        backend.stop_depth_capture_production().unwrap();
        backend.stop_depth_capture_production().unwrap(); // stopping is idempotent
        // The game reaches the copy boundary before its next Present. The lifecycle
        // mutex cannot serialize this ResourceBarrier callback, only the capture mutex can.
        let mut transition = D3D12_RESOURCE_TRANSITION_BARRIER {
            pResource: std::mem::ManuallyDrop::new(Some(source.clone())),
            Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
            StateBefore: D3D12_RESOURCE_STATE_DEPTH_WRITE,
            StateAfter: D3D12_RESOURCE_STATES(0xe0),
        };
        assert!(backend.capture_transition_admitted(
            transition.StateBefore,
            transition.StateAfter,
            transition.Subresource,
            D3D12_RESOURCE_BARRIER_FLAG_NONE,
        ));
        backend.capture_before_depth_write_exit(
            list.as_raw(),
            &transition,
            D3D12_RESOURCE_BARRIER_FLAG_NONE,
            list.vtable().ResourceBarrier as usize,
        );
        unsafe { std::mem::ManuallyDrop::drop(&mut transition.pResource) };
        unsafe { list.Close() }.unwrap();
        assert_eq!(
            backend.capture_slots_in_flight(),
            0,
            "late copy entered after StopRun"
        );
        backend.disarm_depth_capture().unwrap();
        assert!(!backend.state.lock().unwrap().structural_failure);
    }

    #[test]
    fn stop_preserves_recorded_copy_and_lease_until_fenced_drain() {
        use windows::Win32::Graphics::Direct3D12::*;
        let (backend, device, queue, source) = armed_test_capture_with_storage(true);
        let allocator: ID3D12CommandAllocator =
            unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }.unwrap();
        let list: ID3D12GraphicsCommandList = unsafe {
            device.CreateCommandList(
                0,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                &allocator,
                None::<&ID3D12PipelineState>,
            )
        }
        .unwrap();
        backend.begin_depth_capture_interval(652, true);
        {
            let mut guard = backend.capture.lock().unwrap();
            let recorded = unsafe {
                guard.as_mut().unwrap().record_before_depth_write_exit(
                    list.as_raw(),
                    &list,
                    list.vtable().ResourceBarrier as usize,
                    source.as_raw(),
                    Some(7),
                )
            }
            .unwrap();
            assert!(matches!(
                recorded,
                crate::d3d12_capture::RecordResult::Recorded { .. }
            ));
        }
        backend.stop_depth_capture_production().unwrap();
        assert!(
            backend.capture_is_armed(),
            "Execute must still attach the producer fence"
        );
        assert_eq!(backend.capture_slots_in_flight(), 1);
        assert!(
            backend
                .disarm_depth_capture()
                .unwrap_err()
                .contains("in-flight slots")
        );
        unsafe {
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        }
        backend
            .capture
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .on_execute_complete(&queue, &[list.as_raw()])
            .unwrap();
        let snapshots = backend.take_gpu_depth_snapshots().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].present_seq, 652);
        assert!(
            backend.disarm_depth_capture().is_err(),
            "an outstanding lease is not idle"
        );
        backend
            .release_gpu_depth_snapshot(&snapshots[0], None)
            .unwrap();
        crate::d3d12_candidate::tests::wait_until(|| {
            assert!(backend.take_gpu_depth_snapshots().unwrap().is_empty());
            backend.capture_slots_in_flight() == 0
        });
        backend.disarm_depth_capture().unwrap();
        assert!(!backend.capture_is_armed());
        assert!(!backend.state.lock().unwrap().structural_failure);
    }

    #[test]
    fn retired_capture_admission_does_not_latch_a_false_structural_fault() {
        for operation in ["Reset", "Release", "DSV creation"] {
            let backend = armed_test_capture();
            // The callback passed its lock-free admission check. StopRun now wins
            // the capture mutex and removes the idle ring before the callback resumes.
            assert!(backend.capture_is_armed());
            backend.disarm_depth_capture().unwrap();
            if operation == "DSV creation" {
                backend.update_dsv_capture_after_admission(0x3000, None);
            } else {
                backend.discard_recorded_capture_list_after_admission(123, operation);
            }
            assert!(!backend.capture_association_fault.load(Ordering::Acquire));
            assert!(
                !backend.state.lock().unwrap().structural_failure,
                "{operation}"
            );
        }
    }

    #[test]
    fn genuinely_armed_capture_without_a_ring_still_latches_a_fault() {
        for operation in ["Reset", "Release", "DSV creation"] {
            let backend = armed_test_capture();
            let ring = backend.capture.lock().unwrap().take();
            drop(ring);
            assert!(backend.capture_is_armed());
            assert!(backend.stop_depth_capture_production().is_err());
            if operation == "DSV creation" {
                backend.update_dsv_capture_after_admission(0x3000, None);
            } else {
                backend.discard_recorded_capture_list_after_admission(123, operation);
            }
            assert!(backend.capture_association_fault.load(Ordering::Acquire));
            assert!(
                backend.state.lock().unwrap().structural_failure,
                "{operation}"
            );
            backend.abort_depth_capture().unwrap();
        }
    }

    #[test]
    fn candidate_rejections_remain_auditable_without_masking_structural_failure() {
        use windows::Win32::Graphics::Direct3D12::*;
        let backend = armed_test_capture();
        backend
            .state
            .lock()
            .unwrap()
            .reject_candidate("unrelated depth array has unsupported subresource layout");
        assert!(!backend.snapshot().capture_safe);
        assert!(backend.selected_capture_safe());
        for (queue, list, kind) in [
            (0x1000, 0x2000, D3D12_COMMAND_LIST_TYPE_COMPUTE),
            (0x1100, 0x2100, D3D12_COMMAND_LIST_TYPE_COPY),
        ] {
            let mut state = backend.state.lock().unwrap();
            state.register_queue(queue, kind);
            state.register_list(list, kind).unwrap();
            state.execute(queue, &[list as *mut c_void]);
            assert!(
                !state.structural_failure,
                "ordinary non-DIRECT submission must not poison the selected DIRECT depth target"
            );
            drop(state);
            assert!(backend.selected_capture_safe());
        }
        assert_eq!(
            backend.state.lock().unwrap().blocker_counts["queue_not_direct"],
            2
        );
        {
            let _capture_guard = backend.capture.lock().unwrap();
            assert!(
                backend
                    .selected_capture_health()
                    .unwrap_err()
                    .contains("mutex is busy")
            );
        }
        assert!(backend.selected_capture_safe());
        backend.state.lock().unwrap().execute(0xdead, &[]);
        assert!(!backend.selected_capture_safe());
        assert!(
            backend
                .selected_capture_health()
                .unwrap_err()
                .contains("structural failure")
        );
        backend.block("command list has an unhooked interface");
        assert!(!backend.selected_capture_safe());
        backend.disarm_depth_capture().unwrap();
    }

    #[test]
    fn stable_selector_matches_kcd2_without_process_local_candidate_id() {
        let mut state = ObservationState::new();
        state.write_dsv(
            0x3000,
            0x4000,
            Some(DsvMetadata {
                resource_dimension: 3,
                width: 1920,
                height: 1080,
                depth_or_array_size: 1,
                mip_levels: 1,
                sample_count: 1,
                sample_quality: 0,
                resource_format: DXGI_FORMAT_R24G8_TYPELESS.0,
                resource_layout: 0,
                resource_flags: 2,
                dsv_format: DXGI_FORMAT_D24_UNORM_S8_UINT.0,
                dsv_dimension: 3,
                dsv_flags: 0,
                view_desc_explicit: true,
            }),
        );
        let candidate = state.candidates.values_mut().next().unwrap();
        candidate.transition_pairs.insert((16, 128, 0, false), 42);
        let selection = depth_lab::D3d12DepthSelection {
            resource: depth_lab::D3d12ResourceIdentity {
                dimension: 3,
                width: 1920,
                height: 1080,
                depth_or_array_size: 1,
                mip_levels: 1,
                format: depth_lab::DiscoveryDepthFormat::R24G8Typeless,
                sample_count: 1,
                sample_quality: 0,
                layout: 0,
                flags: 2,
            },
            view: depth_lab::D3d12DsvIdentity {
                format: depth_lab::DiscoveryDepthFormat::D24UnormS8Uint,
                dimension: 3,
                flags: 0,
            },
            transition: depth_lab::D3d12TransitionIdentity {
                subresource: 0,
                state_before: 16,
                state_after: 128,
                barrier_flags: 0,
            },
            fallback_draw: None,
        };
        assert!(stable_candidate_matches(candidate, &selection));

        candidate.metadata.view_desc_explicit = false;
        assert!(!stable_candidate_matches(candidate, &selection));
    }

    unsafe extern "system" fn fake_query_interface(
        _this: *mut c_void,
        _iid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        if !out.is_null() {
            unsafe { *out = std::ptr::null_mut() };
        }
        HRESULT(0x8000_4002_u32 as i32)
    }

    unsafe extern "system" fn fake_create_command_queue(
        _this: *mut c_void,
        _desc: *const D3D12_COMMAND_QUEUE_DESC,
        _riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
        if !out.is_null() {
            unsafe { *out = FAKE_QUEUE.load(Ordering::Acquire) as *mut c_void };
        }
        HRESULT(0)
    }

    unsafe extern "system" fn fake_create_command_list(
        _this: *mut c_void,
        _node_mask: u32,
        _kind: D3D12_COMMAND_LIST_TYPE,
        _allocator: *mut c_void,
        _initial_state: *mut c_void,
        _riid: *const GUID,
        out: *mut *mut c_void,
    ) -> HRESULT {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
        if !out.is_null() {
            unsafe { *out = FAKE_LIST.load(Ordering::Acquire) as *mut c_void };
        }
        HRESULT(0)
    }

    unsafe extern "system" fn fake_create_dsv(
        _this: *mut c_void,
        _resource: *mut c_void,
        _desc: *const D3D12_DEPTH_STENCIL_VIEW_DESC,
        _handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "system" fn fake_execute(
        _this: *mut c_void,
        _count: u32,
        _lists: *const *mut c_void,
    ) {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "system" fn fake_release(_this: *mut c_void) -> u32 {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
        1
    }

    unsafe extern "system" fn fake_reset(
        _this: *mut c_void,
        _allocator: *mut c_void,
        _initial_state: *mut c_void,
    ) -> HRESULT {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
        HRESULT(0)
    }

    unsafe extern "system" fn fake_barrier(
        _this: *mut c_void,
        _count: u32,
        _barriers: *const D3D12_RESOURCE_BARRIER,
    ) {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "system" fn fake_rs_set_viewports(
        _this: *mut c_void,
        _count: u32,
        _viewports: *const windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT,
    ) {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "system" fn fake_om_set_render_targets(
        _this: *mut c_void,
        _render_target_count: u32,
        _render_targets: *const D3D12_CPU_DESCRIPTOR_HANDLE,
        _single_range: BOOL,
        _depth: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    ) {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    unsafe extern "system" fn fake_clear_depth(
        _this: *mut c_void,
        _depth: D3D12_CPU_DESCRIPTOR_HANDLE,
        _flags: D3D12_CLEAR_FLAGS,
        _depth_value: f32,
        _stencil: u8,
        _rect_count: u32,
        _rects: *const RECT,
    ) {
        FORWARDED_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    fn leak_fake_object(entry_count: usize, methods: &[(usize, usize)]) -> *mut c_void {
        let mut entries = vec![1usize; entry_count].into_boxed_slice();
        for (index, target) in methods {
            entries[*index] = *target;
        }
        let entries = Box::leak(entries);
        let owner = Box::leak(Box::new(entries.as_mut_ptr()));
        owner as *mut *mut usize as *mut c_void
    }

    fn candidate_retirement_metadata() -> DsvMetadata {
        DsvMetadata {
            resource_dimension: 3,
            width: 1920,
            height: 1080,
            depth_or_array_size: 1,
            mip_levels: 1,
            sample_count: 1,
            sample_quality: 0,
            resource_format: DXGI_FORMAT_R24G8_TYPELESS.0,
            resource_layout: 0,
            resource_flags: 2,
            dsv_format: DXGI_FORMAT_D24_UNORM_S8_UINT.0,
            dsv_dimension: 3,
            dsv_flags: 0,
            view_desc_explicit: true,
        }
    }

    #[test]
    fn candidate_retirement_bounds_descriptor_churn_without_rebinding_selected_identity() {
        let backend = D3d12ObservationBackend::new();
        let mut state = backend.state.lock().unwrap();
        let metadata = Some(candidate_retirement_metadata());
        state.write_dsv(0x3000, 0x4000, metadata);
        let first = state.resolve_handle(0x3000).unwrap();
        backend.arm_selected_depth_hot_path(first);
        state.write_dsv(0x3001, 0x4001, metadata);
        let other = state.resolve_handle(0x3001).unwrap();
        for _ in 0..256 {
            state.write_dsv(0x3000, 0x4000, metadata);
            assert!(state.candidates.len() <= 64);
        }
        let current = state.resolve_handle(0x3000).unwrap();
        assert!(!state.structural_failure);
        assert_eq!(state.candidates_dropped, 0);
        assert!(state.candidates.values().any(|c| c.identity == current));
        assert!(state.candidates.values().any(|c| c.identity == other));
        assert!(!state.candidates.values().any(|c| c.identity == first));
        assert!(!state.identity_is_current(first));
        assert_eq!(backend.selected_depth_token_for(Some(current)), 0);
        assert!(state.capture_source_for_clear(0x2000, first).is_err());
    }

    #[test]
    fn candidate_retirement_preserves_all_current_candidates_and_real_exhaustion() {
        let mut state = ObservationState::new();
        let metadata = Some(candidate_retirement_metadata());
        for index in 0..64 {
            state.write_dsv(0x3000 + index, 0x4000 + index as usize, metadata);
        }
        let identities: Vec<_> = state.candidates.values().map(|c| c.identity).collect();
        state.write_dsv(0x5000, 0x6000, metadata);
        assert_eq!(state.candidates.len(), 64);
        assert_eq!(state.candidates_dropped, 1);
        assert!(state.structural_failure);
        for identity in identities {
            assert!(state.candidates.values().any(|c| c.identity == identity));
        }
        // Later reclamation never rehabilitates the earlier incomplete ledger.
        state.write_dsv(0x3000, 0, None);
        state.write_dsv(0x5001, 0x6001, metadata);
        assert!(state.structural_failure);
    }

    #[test]
    fn candidate_retirement_keeps_recorded_and_bound_identities_until_reset() {
        let mut state = ObservationState::new();
        let metadata = Some(candidate_retirement_metadata());
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2001, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(0x3000, 0x4000, metadata);
        let first = state.resolve_handle(0x3000).unwrap();
        state.record_event(0x2000, ListEvent::BindDepth(Some(first)), "bind");
        state.record_bound_depth(0x2001, Some(first));
        for _ in 0..128 {
            state.write_dsv(0x3000, 0x4000, metadata);
        }
        assert!(state.candidates.values().any(|c| c.identity == first));
        assert!(!state.structural_failure);
        state.execute(0x1000, &[0x2000usize as *mut c_void]);
        assert!(state.structural_failure);
        assert!(
            state
                .blockers
                .iter()
                .any(|message| message.contains("stale DSV"))
        );
        state.reset_list(0x2001);
        state.retire_unused_candidates();
        // A submitted list is still replayable until Reset.
        assert!(state.candidates.values().any(|c| c.identity == first));
        state.reset_list(0x2000);
        state.retire_unused_candidates();
        assert!(!state.candidates.values().any(|c| c.identity == first));
        assert!(state.structural_failure);
    }

    #[test]
    fn candidate_retirement_keeps_pending_clear_statistics_until_present() {
        let mut state = ObservationState::new();
        let metadata = Some(candidate_retirement_metadata());
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(0x3000, 0x4000, metadata);
        let first = state.resolve_handle(0x3000).unwrap();
        state.record_event(
            0x2000,
            ListEvent::ClearDepth {
                identity: Some(first),
                flags: D3D12_CLEAR_FLAG_DEPTH.0,
                depth_value_bits: 0,
                stencil: 0,
                rect_count: 0,
                same_list_capture_eligible: true,
            },
            "clear",
        );
        state.execute(0x1000, &[0x2000usize as *mut c_void]);
        state.reset_list(0x2000);
        for _ in 0..128 {
            state.write_dsv(0x3000, 0x4000, metadata);
        }
        let old = state
            .candidates
            .values()
            .find(|c| c.identity == first)
            .unwrap();
        assert_eq!(old.pending_clears, 1);
        assert_eq!(old.pending_same_list_capture_eligible_clears, 1);
        state.note_present();
        let old = state
            .candidates
            .values()
            .find(|c| c.identity == first)
            .unwrap();
        assert_eq!(old.present_intervals_with_clear, 1);
        assert_eq!(
            old.present_intervals_with_same_list_capture_eligible_clear,
            1
        );
        state.retire_unused_candidates();
        assert!(!state.candidates.values().any(|c| c.identity == first));
        assert!(!state.structural_failure);
    }

    #[test]
    fn candidate_retirement_preserves_current_descriptor_alias_keys() {
        let mut state = ObservationState::new();
        let metadata = Some(candidate_retirement_metadata());
        state.write_dsv(0x3000, 0x4000, metadata);
        let representative = state.resolve_handle(0x3000).unwrap();
        state.write_dsv(0x3001, 0x4000, metadata);
        let alias = state.resolve_handle(0x3001).unwrap();
        for _ in 0..128 {
            state.write_dsv(0x3000, 0x4000, metadata);
        }
        assert!(!state.identity_is_current(representative));
        assert!(state.identity_is_current(alias));
        assert!(
            state
                .candidates
                .values()
                .any(|c| c.identity == representative)
        );
        assert!(!state.structural_failure);
    }

    #[test]
    fn descriptor_overwrite_is_fail_closed_at_execution() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(0x3000, 0x4000, None);
        let first = state.resolve_handle(0x3000).unwrap();
        state.record_event(0x2000, ListEvent::BindDepth(Some(first)), "bind");
        state.record_event(
            0x2000,
            ListEvent::ClearDepth {
                identity: Some(first),
                flags: D3D12_CLEAR_FLAG_DEPTH.0,
                depth_value_bits: 1.0f32.to_bits(),
                stencil: 0,
                rect_count: 0,
                same_list_capture_eligible: false,
            },
            "clear",
        );
        state.write_dsv(0x3000, 0x5000, None);

        state.execute(0x1000, &[0x2000usize as *mut c_void]);

        assert_eq!(state.descriptor_overwrites, 1);
        assert_eq!(state.eligible_pre_clear_snapshots, 0);
        assert!(state.blockers.iter().any(|value| value.contains("stale")));
    }

    #[test]
    fn resource_release_invalidates_all_descriptors_for_that_identity() {
        let mut state = ObservationState::new();
        state.write_dsv(0x3000, 0x4000, None);
        state.write_dsv(0x3001, 0x4000, None);
        assert!(state.resolve_handle(0x3000).is_some());
        assert!(state.resolve_handle(0x3001).is_some());

        state.release_resource(0x4000);

        assert_eq!(state.resolve_handle(0x3000), None);
        assert_eq!(state.resolve_handle(0x3001), None);
    }

    #[test]
    fn a_unique_bound_depth_write_clear_becomes_observation_candidate() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(0x3000, 0x4000, None);
        let identity = state.resolve_handle(0x3000).unwrap();
        state.record_event(
            0x2000,
            ListEvent::Transition(RawTransition {
                resource: 0x4000,
                before: D3D12_RESOURCE_STATE_COMMON,
                after: D3D12_RESOURCE_STATE_DEPTH_WRITE,
                subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                split: false,
            }),
            "barrier",
        );
        state.record_event(0x2000, ListEvent::BindDepth(Some(identity)), "bind");
        state.record_event(
            0x2000,
            ListEvent::ClearDepth {
                identity: Some(identity),
                flags: D3D12_CLEAR_FLAG_DEPTH.0,
                depth_value_bits: 1.0f32.to_bits(),
                stencil: 0,
                rect_count: 0,
                same_list_capture_eligible: true,
            },
            "clear",
        );

        state.execute(0x1000, &[0x2000usize as *mut c_void]);

        assert_eq!(state.eligible_pre_clear_snapshots, 1);
        assert!(state.blockers.is_empty(), "{:?}", state.blockers);
    }

    #[test]
    fn cp2077_composed_read_states_can_return_to_depth_write_before_an_unbound_clear() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(
            0x3000,
            0x4000,
            Some(DsvMetadata {
                resource_dimension: 3,
                width: 1280,
                height: 720,
                depth_or_array_size: 1,
                mip_levels: 1,
                sample_count: 1,
                sample_quality: 0,
                resource_format: 39,
                resource_layout: 0,
                resource_flags: 2,
                dsv_format: 40,
                dsv_dimension: 3,
                dsv_flags: 0,
                view_desc_explicit: true,
            }),
        );
        let identity = state.resolve_handle(0x3000).unwrap();
        for (before, after) in [(0xE0, 0x40), (0x40, D3D12_RESOURCE_STATE_DEPTH_WRITE.0)] {
            let transition = RawTransition {
                resource: 0x4000,
                before: D3D12_RESOURCE_STATES(before),
                after: D3D12_RESOURCE_STATES(after),
                subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                split: false,
            };
            state.record_transition_state(0x2000, transition);
            state.record_event(0x2000, ListEvent::Transition(transition), "barrier");
        }
        assert!(state.capture_source_for_clear(0x2000, identity).is_ok());
        state.record_event(
            0x2000,
            ListEvent::ClearDepth {
                identity: Some(identity),
                flags: D3D12_CLEAR_FLAG_DEPTH.0,
                depth_value_bits: 0.0f32.to_bits(),
                stencil: 0,
                rect_count: 0,
                same_list_capture_eligible: true,
            },
            "clear",
        );

        state.execute(0x1000, &[0x2000usize as *mut c_void]);
        let candidate = &state.candidate_observations()[0];
        assert_eq!(candidate.bind_calls, 0);
        assert_eq!(candidate.clear_while_bound_calls, 0);
        assert_eq!(candidate.eligible_clear_calls, 1);
        assert_eq!(candidate.same_list_capture_eligible_clear_calls, 1);
    }

    #[test]
    fn queue_identity_and_resource_epoch_do_not_silently_drift() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_queue(0x1001, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.execute(0x1000, &[0x2000usize as *mut c_void]);
        state.execute(0x1001, &[0x2000usize as *mut c_void]);
        assert!(
            state
                .blockers
                .iter()
                .any(|value| value.contains("moved from queue"))
        );

        state.write_dsv(0x3000, 0x4000, None);
        let old = state.resolve_handle(0x3000).unwrap();
        state.resource_epoch += 1;
        assert!(!state.identity_is_current(old));
    }

    #[test]
    fn composed_read_state_is_preserved_without_becoming_capture_eligible() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(0x3000, 0x4000, None);
        state.record_event(
            0x2000,
            ListEvent::Transition(RawTransition {
                resource: 0x4000,
                before: D3D12_RESOURCE_STATE_COMMON,
                after: D3D12_RESOURCE_STATES(0x40),
                subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                split: false,
            }),
            "barrier",
        );
        state.execute(0x1000, &[0x2000usize as *mut c_void]);
        assert_eq!(state.resources[&0x4000].raw_state, Some(0x40));
        assert_eq!(state.eligible_pre_clear_snapshots, 0);
        assert!(state.blockers.is_empty());
    }

    #[test]
    fn discovery_report_uses_anonymous_ids_and_bounded_blocker_classes() {
        let mut state = ObservationState::new();
        state.register_queue(0x1000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.register_list(0x2000, D3D12_COMMAND_LIST_TYPE_DIRECT);
        state.write_dsv(
            0x3000,
            0x4000,
            Some(DsvMetadata {
                resource_dimension: 3,
                width: 1280,
                height: 720,
                depth_or_array_size: 1,
                mip_levels: 1,
                sample_count: 1,
                sample_quality: 0,
                resource_format: 39,
                resource_layout: 0,
                resource_flags: 2,
                dsv_format: 40,
                dsv_dimension: 3,
                dsv_flags: 0,
                view_desc_explicit: true,
            }),
        );
        let identity = state.resolve_handle(0x3000).unwrap();
        state.record_event(0x2000, ListEvent::BindDepth(Some(identity)), "bind");
        state.record_event(
            0x2000,
            ListEvent::ClearDepth {
                identity: Some(identity),
                flags: D3D12_CLEAR_FLAG_DEPTH.0,
                depth_value_bits: 0.0f32.to_bits(),
                stencil: 0,
                rect_count: 0,
                same_list_capture_eligible: false,
            },
            "clear",
        );
        assert_eq!(state.candidate_observations()[0].pending_clear_calls, 0);
        state.execute(0x1000, &[0x2000usize as *mut c_void]);
        assert_eq!(state.candidate_observations()[0].pending_clear_calls, 1);
        assert_eq!(
            state.candidate_observations()[0].last_present_call_with_clear,
            None
        );
        state.note_present();
        assert_eq!(state.candidate_observations()[0].pending_clear_calls, 0);
        assert_eq!(
            state.candidate_observations()[0].last_present_call_with_clear,
            Some(1)
        );
        state.note_present();
        assert_eq!(state.candidate_observations()[0].pending_clear_calls, 0);
        assert_eq!(
            state.candidate_observations()[0].last_present_call_with_clear,
            Some(1)
        );

        let report = serde_json::to_string(&state.candidate_observations()).unwrap();
        assert!(!report.contains("0x1000"));
        assert!(!report.contains("0x2000"));
        assert!(!report.contains("0x3000"));
        assert!(!report.contains("0x4000"));
        assert_eq!(state.blocker_counts["clear_without_proven_depth_write"], 1);
        let candidate = &state.candidate_observations()[0];
        assert_eq!(candidate.bind_calls, 1);
        assert_eq!(candidate.clear_while_bound_calls, 1);
        assert_eq!(candidate.present_intervals_with_one_clear, 1);
        assert_eq!(candidate.clear_parameters[0].depth_value_bits, 0);
    }

    #[test]
    fn dummy_com_vtables_install_forward_and_observe_fail_closed() {
        FORWARDED_CALLS.store(0, Ordering::Release);
        let queue = leak_fake_object(
            d3d12_abi::COMMAND_QUEUE_VTABLE_ENTRIES,
            &[(
                d3d12_abi::command_queue::EXECUTE_COMMAND_LISTS,
                fake_execute as *const () as usize,
            )],
        );
        let list = leak_fake_object(
            d3d12_abi::GRAPHICS_COMMAND_LIST_VTABLE_ENTRIES,
            &[
                (
                    d3d12_abi::graphics_command_list::RS_SET_VIEWPORTS,
                    fake_rs_set_viewports as *const () as usize,
                ),
                (
                    d3d12_abi::graphics_command_list::RESET,
                    fake_reset as *const () as usize,
                ),
                (
                    d3d12_abi::graphics_command_list::RESOURCE_BARRIER,
                    fake_barrier as *const () as usize,
                ),
                (
                    d3d12_abi::graphics_command_list::OM_SET_RENDER_TARGETS,
                    fake_om_set_render_targets as *const () as usize,
                ),
                (
                    d3d12_abi::graphics_command_list::CLEAR_DEPTH_STENCIL_VIEW,
                    fake_clear_depth as *const () as usize,
                ),
            ],
        );
        FAKE_QUEUE.store(queue as usize, Ordering::Release);
        FAKE_LIST.store(list as usize, Ordering::Release);
        let device = leak_fake_object(
            d3d12_abi::DEVICE_VTABLE_ENTRIES,
            &[
                (
                    iunknown_query_interface_index(),
                    fake_query_interface as *const () as usize,
                ),
                (
                    d3d12_abi::device::CREATE_COMMAND_QUEUE,
                    fake_create_command_queue as *const () as usize,
                ),
                (
                    d3d12_abi::device::CREATE_COMMAND_LIST,
                    fake_create_command_list as *const () as usize,
                ),
                (
                    d3d12_abi::device::CREATE_DEPTH_STENCIL_VIEW,
                    fake_create_dsv as *const () as usize,
                ),
            ],
        );
        let resource = leak_fake_object(
            d3d12_abi::RESOURCE_VTABLE_ENTRIES,
            &[(iunknown_release_index(), fake_release as *const () as usize)],
        );

        let backend = D3d12ObservationBackend::new();
        unsafe { backend.install(device) }.unwrap();

        let queue_desc = D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        };
        let mut created_queue = std::ptr::null_mut();
        let create_queue: CreateCommandQueueFn = unsafe {
            std::mem::transmute(
                *(*device.cast::<*mut usize>()).add(d3d12_abi::device::CREATE_COMMAND_QUEUE),
            )
        };
        assert!(
            unsafe {
                create_queue(
                    device,
                    &queue_desc,
                    &ID3D12CommandQueue::IID,
                    &mut created_queue,
                )
            }
            .is_ok()
        );
        assert_eq!(created_queue, queue);

        let mut created_list = std::ptr::null_mut();
        let create_list: CreateCommandListFn = unsafe {
            std::mem::transmute(
                *(*device.cast::<*mut usize>()).add(d3d12_abi::device::CREATE_COMMAND_LIST),
            )
        };
        assert!(
            unsafe {
                create_list(
                    device,
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &ID3D12GraphicsCommandList::IID,
                    &mut created_list,
                )
            }
            .is_ok()
        );
        assert_eq!(created_list, list);

        let create_dsv: CreateDepthStencilViewFn = unsafe {
            std::mem::transmute(
                *(*device.cast::<*mut usize>()).add(d3d12_abi::device::CREATE_DEPTH_STENCIL_VIEW),
            )
        };
        let handle = D3D12_CPU_DESCRIPTOR_HANDLE { ptr: 0x3000 };
        unsafe { create_dsv(device, resource, std::ptr::null(), handle) };

        let bind: OmSetRenderTargetsFn = unsafe {
            std::mem::transmute(
                *(*list.cast::<*mut usize>())
                    .add(d3d12_abi::graphics_command_list::OM_SET_RENDER_TARGETS),
            )
        };
        unsafe { bind(list, 0, std::ptr::null(), BOOL(0), &handle) };
        let clear: ClearDepthStencilViewFn = unsafe {
            std::mem::transmute(
                *(*list.cast::<*mut usize>())
                    .add(d3d12_abi::graphics_command_list::CLEAR_DEPTH_STENCIL_VIEW),
            )
        };
        unsafe {
            clear(
                list,
                handle,
                D3D12_CLEAR_FLAGS(1),
                1.0,
                0,
                0,
                std::ptr::null(),
            )
        };
        let execute: ExecuteCommandListsFn = unsafe {
            std::mem::transmute(
                *(*queue.cast::<*mut usize>()).add(d3d12_abi::command_queue::EXECUTE_COMMAND_LISTS),
            )
        };
        unsafe { execute(queue, 1, &list) };

        let inactive_snapshot = backend.snapshot();
        assert_eq!(FORWARDED_CALLS.load(Ordering::Acquire), 6);
        assert_eq!(inactive_snapshot.clear_depth_calls, 0);
        assert_eq!(inactive_snapshot.execute_command_lists_calls, 0);

        let set_viewports: unsafe extern "system" fn(
            *mut c_void,
            u32,
            *const windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT,
        ) = unsafe {
            std::mem::transmute(
                *(*list.cast::<*mut usize>())
                    .add(d3d12_abi::graphics_command_list::RS_SET_VIEWPORTS),
            )
        };
        assert_ne!(
            set_viewports as *const (),
            fake_rs_set_viewports as *const ()
        );
        let viewport = windows::Win32::Graphics::Direct3D12::D3D12_VIEWPORT {
            Width: 1920.0,
            Height: 1080.0,
            MaxDepth: 1.0,
            ..Default::default()
        };
        unsafe { set_viewports(list, 1, &viewport) };
        assert_eq!(FORWARDED_CALLS.load(Ordering::Acquire), 7);

        backend.set_runtime_observation_active(true);
        unsafe { set_viewports(list, 1, &viewport) };
        unsafe { bind(list, 0, std::ptr::null(), BOOL(0), &handle) };
        unsafe {
            clear(
                list,
                handle,
                D3D12_CLEAR_FLAGS(1),
                1.0,
                0,
                0,
                std::ptr::null(),
            )
        };
        unsafe { execute(queue, 1, &list) };

        let snapshot = backend.snapshot();
        assert_eq!(FORWARDED_CALLS.load(Ordering::Acquire), 11);
        assert_eq!(snapshot.queue_count, 1);
        assert_eq!(snapshot.command_list_count, 1);
        assert_eq!(snapshot.create_dsv_calls, 1);
        assert_eq!(snapshot.clear_depth_calls, 1);
        assert_eq!(snapshot.execute_command_lists_calls, 1);
        assert!(!snapshot.capture_safe);
        assert_eq!(snapshot.eligible_pre_clear_snapshots, 0);
        assert!(snapshot.snapshot_copy_implemented);
        assert!(snapshot.fence_readback_implemented);
        assert!(!snapshot.capture_armed);
        assert_eq!(snapshot.capture_in_flight, 0);
        assert!(
            snapshot
                .blockers
                .iter()
                .any(|value| value.contains("no proven DEPTH_WRITE state"))
        );
    }

    #[test]
    fn hot_list_registry_filters_non_selected_draws_without_a_tracker_lock() {
        let registry = HotListRegistry::new();
        registry.register(0x2000, 9);
        registry.bind_heaps(0x2000, [0x3000, 0]);
        registry.bind_root(0x2000, 3, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        assert!(registry.selected_draw(0x2000, 7).is_none());

        registry.bind_selected_depth(0x2000, 7);
        let draw = registry.selected_draw(0x2000, 7).unwrap();
        assert_eq!(draw.list_generation, 1);
        assert_eq!(draw.draw_ordinal, 2);
        assert_eq!(draw.descriptor_heaps, [0x3000, 0]);
        assert_eq!(draw.roots.len(), 1);
        assert_eq!(draw.roots[0].source, RootBindingSource::DescriptorTableBase);

        registry.reset(0x2000);
        assert!(registry.selected_draw(0x2000, 7).is_none());
        assert_eq!(registry.drops.load(Ordering::Acquire), 0);
    }

    #[test]
    fn color_only_om_binds_do_not_shift_the_sealed_kcd2_player_pass() {
        let registry = HotListRegistry::new();
        registry.register(0x2000, 9);
        let plan = depth_lab::kcd2_pose::Kcd2LivePosePlan::pinned();
        let player_pass = plan.horizontal_player_root_binding.pass_ordinal;
        for pass in 0..=player_pass {
            registry.unbind_depth(0x2000);
            assert!(registry.selected_draw_ordinals(0x2000, 7).is_none());
            registry.unbind_depth(0x2000);
            registry.bind_selected_depth(0x2000, if pass == player_pass { 7 } else { 0 });
            if pass < player_pass {
                assert!(registry.selected_draw_ordinals(0x2000, 7).is_none());
            }
        }
        let (_, pass, draw) = registry.selected_draw_ordinals(0x2000, 7).unwrap();
        assert_eq!((pass, draw), (player_pass, 0));
        registry.mark_fallback_draw_proven(0x2000);
        registry.unbind_depth(0x2000);
        assert!(!registry.fallback_draw_proven(0x2000));
        assert!(registry.selected_draw_ordinals(0x2000, 7).is_none());
        registry.bind_selected_depth(0x2000, 7);
        let (_, pass, draw) = registry.selected_draw_ordinals(0x2000, 7).unwrap();
        assert_eq!((pass, draw), (player_pass + 1, 0));
        registry.reset(0x2000);
        registry.unbind_depth(0x2000);
        registry.bind_selected_depth(0x2000, 7);
        let (_, pass, draw) = registry.selected_draw_ordinals(0x2000, 7).unwrap();
        assert_eq!((pass, draw), (0, 0));
    }

    #[test]
    fn copy_draw_stamp_tracks_recording_and_admits_missing_hot_lists() {
        let registry = HotListRegistry::new();
        assert_eq!(registry.recorded_draw_count(0), None);
        assert_eq!(registry.recorded_draw_count(0x2000), None);
        registry.register(0x2000, 9);
        assert_eq!(registry.recorded_draw_count(0x2000), Some(0));
        // The hot counter includes unselected draws. The legacy tracker's
        // observe_draw counter is independent and cannot be compared to it.
        assert_eq!(registry.selected_draw_ordinals(0x2000, 7), None);
        assert_eq!(registry.recorded_draw_count(0x2000), Some(1));
        registry.bind_selected_depth(0x2000, 7);
        assert_eq!(registry.selected_draw_ordinals(0x2000, 7), Some((2, 0, 0)));
        let stamp = registry.recorded_draw_count(0x2000);
        registry.bind_selected_depth(0x2000, 7);
        assert_eq!(registry.selected_draw_ordinals(0x2000, 7), Some((3, 1, 0)));
        assert_eq!(stamp, Some(2), "rebinding a pass must not reset list order");
        registry.reset(0x2000);
        assert_eq!(registry.recorded_draw_count(0x2000), Some(0));
        for count in [u64::from(u32::MAX), u64::from(u32::MAX) + 1] {
            registry
                .find(0x2000)
                .unwrap()
                .draw_ordinal
                .store(count, Ordering::Release);
            assert_eq!(registry.recorded_draw_count(0x2000), None);
        }
        registry.release(0x2000);
        assert_eq!(registry.recorded_draw_count(0x2000), None);
        registry.register(0x2000, 10);
        assert_eq!(registry.recorded_draw_count(0x2000), Some(0));
    }

    // Exercise the real hot-counter -> bindings -> tracker -> observation path
    // entirely in memory, for both root binding kinds and either mark timing.
    fn assert_same_list_copy_draw_stamp(late_mark: bool) {
        for root_kind in [ROOT_DESCRIPTOR_TABLE, ROOT_DIRECT_CBV] {
            let registry = HotListRegistry::new();
            let mut tracker = RenderCameraTracker::new(1280, 720, 75.0);
            let list = 0x2000;
            let queue = QueueId(5);
            tracker.register_buffer(0x1000, 0x10_0000, 512);
            tracker.map_buffer(0x1000, 0x20_0000);
            tracker.register_list(list, 9);
            tracker.arm_selected_depth(DsvIdentity {
                handle: DescriptorHandle(4),
                descriptor_generation: 2,
                resource: ResourceId(7),
                resource_epoch: 3,
            });
            registry.register(list, 9);
            registry.bind_selected_depth(list, 7);
            if root_kind == ROOT_DESCRIPTOR_TABLE {
                tracker.register_descriptor_heap(0x3000, 0x40_0000, 0x50_0000, 8, 32, true);
                tracker.register_root_signature_with_fingerprint(
                    0x6000,
                    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .into(),
                    BTreeMap::from([(
                        4,
                        vec![RootCbvRange {
                            table_offset: 2,
                            descriptor_count: 1,
                            descriptor_range_index: 0,
                            base_shader_register: 0,
                            register_space: 0,
                        }],
                    )]),
                );
                tracker.write_cbv_descriptor(0x40_0040, 0x10_0000, 256);
                registry.bind_heaps(list, [0x3000, 0]);
                registry.bind_root_signature(list, 0x6000);
                registry.bind_root(list, 4, root_kind, 0x50_0000);
            } else {
                registry.bind_root(list, 4, root_kind, 0x10_0000);
            }
            let mut stamp = None;
            for draw_number in 1..=2 {
                let draw = registry.selected_draw(list, 7).unwrap();
                assert_eq!(draw.draw_ordinal, draw_number);
                // Different view translations keep both observations visible.
                let x = 1.0 / (75.0_f32.to_radians() * 0.5).tan();
                let matrices: [[f32; 4]; 8] = [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [draw_number as f32, 2.0, 3.0, 1.0],
                    [x, 0.0, 0.0, 0.0],
                    [0.0, x * 16.0 / 9.0, 0.0, 0.0],
                    [0.0, 0.0, 0.0001, 1.0],
                    [0.0, 0.0, 0.1, 0.0],
                ];
                let mut bytes: Vec<u8> = matrices
                    .into_iter()
                    .flatten()
                    .flat_map(f32::to_le_bytes)
                    .collect();
                bytes.resize(512, 0);
                tracker
                    .observe_selected_draw(list, &draw, |_, _, size| Some(bytes[..size].to_vec()));
                if draw_number == 1 {
                    // draw A -> copy recording stamp -> draw B, same recording.
                    stamp = registry.recorded_draw_count(list);
                }
            }
            assert_eq!(stamp, Some(1));
            assert_eq!(registry.recorded_draw_count(list), Some(2));
            tracker.execute(queue, &[list]);
            if !late_mark {
                tracker.mark_depth_copy_submitted_in_list(queue, 44, Some(list), Some(0), stamp);
            }
            tracker.present(queue, 44, 1234);
            if late_mark {
                let before = tracker.snapshot();
                assert_eq!(before.observations.len(), 2);
                for draw in &before.observations {
                    assert_eq!(draw.depth_copy_draws_before_in_list, None);
                    assert!(!draw.copy_before_clear_submitted);
                    assert!(!draw.selected_present_identity_passed);
                }
                tracker.mark_depth_copy_submitted_in_list(queue, 44, Some(list), Some(0), stamp);
            }
            let snapshot = tracker.snapshot();
            assert_eq!(snapshot.observations.len(), 2);
            let a = &snapshot.observations[0];
            let b = &snapshot.observations[1];
            for draw in [a, b] {
                assert_eq!(draw.command_list_tag, 9);
                assert_eq!(draw.command_list_generation, 1);
                assert_eq!(draw.depth_copy_execute_batch, Some(draw.execute_batch));
                assert_eq!(
                    draw.depth_copy_list_index_in_batch,
                    Some(draw.list_index_in_batch)
                );
                assert_eq!(
                    draw.depth_copy_command_list_tag,
                    Some(draw.command_list_tag)
                );
                assert_eq!(draw.depth_copy_draws_before_in_list, stamp);
                assert_eq!(
                    serde_json::to_value(draw).unwrap()["depth_copy_draws_before_in_list"],
                    1,
                );
            }
            assert!(a.draw_ordinal <= stamp.unwrap());
            assert!(stamp.unwrap() < b.draw_ordinal);
            // A ran before the copy and is admitted for this Present's depth; B
            // was recorded after it in the same list and renders the next frame.
            assert!(a.copy_before_clear_submitted && a.selected_present_identity_passed);
            assert!(!b.copy_before_clear_submitted && !b.selected_present_identity_passed);
        }
    }

    #[test]
    fn same_list_depth_copy_draw_stamp_orders_draws_at_present() {
        assert_same_list_copy_draw_stamp(false);
    }

    #[test]
    fn same_list_depth_copy_draw_stamp_backfills_existing_observations() {
        assert_same_list_copy_draw_stamp(true);
    }

    #[test]
    fn fallback_draw_ordinal_restarts_for_each_selected_dsv_pass() {
        let registry = HotListRegistry::new();
        registry.register(0x2000, 9);
        registry.bind_selected_depth(0x2000, 7);
        assert_eq!(registry.selected_draw_ordinals(0x2000, 7), Some((1, 0, 0)));
        assert_eq!(registry.selected_draw_ordinals(0x2000, 7), Some((2, 0, 1)));
        registry.mark_fallback_draw_proven(0x2000);
        assert!(registry.fallback_draw_proven(0x2000));

        registry.bind_selected_depth(0x2000, 7);
        assert!(!registry.fallback_draw_proven(0x2000));
        assert_eq!(registry.selected_draw_ordinals(0x2000, 7), Some((3, 1, 0)));
    }

    #[test]
    fn redundant_root_and_heap_sets_preserve_direct_and_table_arguments() {
        let registry = HotListRegistry::new();
        registry.register(0x2000, 9);
        registry.bind_selected_depth(0x2000, 7);
        registry.bind_root_signature(0x2000, 0x6000);
        registry.bind_heaps(0x2000, [0x3000, 0x4000]);
        registry.bind_root(0x2000, 2, ROOT_DIRECT_CBV, 0xa0000);
        registry.bind_root(0x2000, 3, ROOT_DESCRIPTOR_TABLE, 0x50000);
        registry.bind_root_signature(0x2000, 0x6000);
        registry.bind_heaps(0x2000, [0x3000, 0x4000]);
        let draw = registry.selected_draw(0x2000, 7).unwrap();
        assert_eq!(draw.roots.len(), 2);
        assert_eq!(draw.roots[0].source, RootBindingSource::Direct);
        assert_eq!(draw.roots[0].value, 0xa0000);
        assert_eq!(draw.roots[1].value, 0x50000);
        registry.bind_heaps(0x2000, [0x3001, 0x4000]);
        let draw = registry.selected_draw(0x2000, 7).unwrap();
        assert_eq!(draw.roots.len(), 1);
        assert_eq!(draw.roots[0].source, RootBindingSource::Direct);
        registry.bind_root_signature(0x2000, 0x6001);
        assert!(registry.selected_draw(0x2000, 7).unwrap().roots.is_empty());
        registry.bind_root(0x2000, 2, ROOT_DIRECT_CBV, 0xb0000);
        registry.reset(0x2000);
        assert_eq!(registry.root_signature(0x2000), Some(0));
        registry.bind_selected_depth(0x2000, 7);
        assert!(registry.selected_draw(0x2000, 7).unwrap().roots.is_empty());
    }

    #[test]
    fn heap_and_root_signature_changes_invalidate_descriptor_table_arguments() {
        let registry = HotListRegistry::new();
        registry.register(0x2000, 9);
        registry.bind_selected_depth(0x2000, 7);
        registry.bind_root(0x2000, 3, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        registry.bind_heaps(0x2000, [0x3000, 0]);
        assert!(registry.selected_draw(0x2000, 7).unwrap().roots.is_empty());

        registry.bind_root(0x2000, 3, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        registry.bind_root_signature(0x2000, 0x6000);
        assert!(registry.selected_draw(0x2000, 7).unwrap().roots.is_empty());
    }

    #[test]
    fn serialized_root_signature_yields_only_its_declared_cbv_offsets() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_DESCRIPTOR_RANGE_TYPE_SRV, D3D12_DESCRIPTOR_RANGE1, D3D12_ROOT_DESCRIPTOR_TABLE1,
            D3D12_ROOT_PARAMETER1, D3D12_ROOT_PARAMETER1_0, D3D12_ROOT_SIGNATURE_DESC1,
            D3D12_VERSIONED_ROOT_SIGNATURE_DESC, D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0,
            D3D12SerializeVersionedRootSignature,
        };

        let ranges = [
            D3D12_DESCRIPTOR_RANGE1 {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 2,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
                ..Default::default()
            },
            D3D12_DESCRIPTOR_RANGE1 {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
                NumDescriptors: 1,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
                ..Default::default()
            },
            D3D12_DESCRIPTOR_RANGE1 {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
                NumDescriptors: 2,
                BaseShaderRegister: 1,
                OffsetInDescriptorsFromTableStart: 7,
                ..Default::default()
            },
        ];
        let table = D3D12_ROOT_DESCRIPTOR_TABLE1 {
            NumDescriptorRanges: ranges.len() as u32,
            pDescriptorRanges: ranges.as_ptr(),
        };
        let parameter = D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                DescriptorTable: table,
            },
            ..Default::default()
        };
        let signature = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: 1,
                    pParameters: &parameter,
                    ..Default::default()
                },
            },
        };
        let mut blob = None;
        unsafe { D3D12SerializeVersionedRootSignature(&signature, &mut blob, None) }.unwrap();
        let blob = blob.unwrap();
        let parsed = parse_root_signature_cbv_ranges(unsafe { blob.GetBufferPointer() }, unsafe {
            blob.GetBufferSize()
        })
        .unwrap();
        assert_eq!(
            parsed[&0],
            vec![
                RootCbvRange {
                    table_offset: 2,
                    descriptor_count: 1,
                    descriptor_range_index: 1,
                    base_shader_register: 0,
                    register_space: 0,
                },
                RootCbvRange {
                    table_offset: 7,
                    descriptor_count: 2,
                    descriptor_range_index: 2,
                    base_shader_register: 1,
                    register_space: 0,
                }
            ]
        );
    }

    #[test]
    fn bindless_non_cbv_table_keeps_other_bounded_cbv_ranges_observable() {
        use windows::Win32::Graphics::Direct3D12::{
            D3D12_DESCRIPTOR_RANGE_TYPE_SRV, D3D12_DESCRIPTOR_RANGE1, D3D12_ROOT_DESCRIPTOR_TABLE1,
            D3D12_ROOT_PARAMETER1, D3D12_ROOT_PARAMETER1_0, D3D12_ROOT_SIGNATURE_DESC1,
            D3D12_VERSIONED_ROOT_SIGNATURE_DESC, D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0,
            D3D12SerializeVersionedRootSignature,
        };

        let bindless_range = D3D12_DESCRIPTOR_RANGE1 {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: u32::MAX,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            ..Default::default()
        };
        let bounded_cbv_range = D3D12_DESCRIPTOR_RANGE1 {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
            NumDescriptors: 2,
            OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            ..Default::default()
        };
        let tables = [
            D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: 1,
                pDescriptorRanges: &bindless_range,
            },
            D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: 1,
                pDescriptorRanges: &bounded_cbv_range,
            },
        ];
        let parameters = tables.map(|table| D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                DescriptorTable: table,
            },
            ..Default::default()
        });
        let signature = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: parameters.len() as u32,
                    pParameters: parameters.as_ptr(),
                    ..Default::default()
                },
            },
        };
        let mut blob = None;
        unsafe { D3D12SerializeVersionedRootSignature(&signature, &mut blob, None) }.unwrap();
        let blob = blob.unwrap();
        let parsed = parse_root_signature_cbv_ranges(unsafe { blob.GetBufferPointer() }, unsafe {
            blob.GetBufferSize()
        })
        .unwrap();

        assert_eq!(
            parsed,
            BTreeMap::from([(
                1,
                vec![RootCbvRange {
                    table_offset: 0,
                    descriptor_count: 2,
                    descriptor_range_index: 0,
                    base_shader_register: 0,
                    register_space: 0,
                }]
            )])
        );
    }

    #[test]
    fn unbounded_cbv_range_is_identity_only_and_never_enumerated() {
        let mut output = BTreeMap::new();
        let mut table_cursor = Some(0);
        let mut total_cbv_descriptors = 0;

        append_root_cbv_range(
            RootCbvRangeBuildContext {
                output: &mut output,
                table_cursor: &mut table_cursor,
                total_cbv_descriptors: &mut total_cbv_descriptors,
            },
            RootDescriptorRangeDeclaration {
                root_parameter: 3,
                range_type: D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
                descriptor_count: u32::MAX,
                declared_offset: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
                descriptor_range_index: 0,
                base_shader_register: 0,
                register_space: 0,
            },
        )
        .unwrap();

        assert!(output.is_empty());
        assert_eq!(table_cursor, None);
        assert_eq!(total_cbv_descriptors, 0);
    }

    #[test]
    fn hot_list_accepts_the_maximum_d3d12_root_parameter_without_drops() {
        let registry = HotListRegistry::new();
        registry.register(0x2000, 9);
        registry.bind_selected_depth(0x2000, 7);
        registry.bind_root(0x2000, 63, ROOT_DESCRIPTOR_TABLE, 0x50_0000);
        let draw = registry.selected_draw(0x2000, 7).unwrap();
        assert_eq!(draw.roots[0].root_parameter_index, 63);
        assert_eq!(registry.drops.load(Ordering::Acquire), 0);
    }
}

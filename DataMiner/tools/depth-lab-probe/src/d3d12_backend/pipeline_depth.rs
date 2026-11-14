//! PSO depth-state observations only. Creation/release use a separate registry;
//! binding does not look up that registry or acquire the render-camera mutex.
mod details;

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::mem::{align_of, offset_of, size_of};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC};
use windows::core::{GUID, HRESULT, IUnknown, Interface, PCWSTR};

use super::{
    D3d12ObservationBackend, HookMethod, backend, iunknown_release_index, readable_mapped_range,
};
use crate::d3d12_render_camera::draw_pass::PipelineDepthTest;
use crate::d3d12_render_camera::{PipelineDepthState, SelectedDrawBindings};
use crate::hook_timing::TimingSnapshot;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PsoObservation {
    depth: PipelineDepthState,
    test: Option<PipelineDepthTest>,
}

const MAX_STREAM_BYTES: usize = 64 * 1024;
const MAX_SUBOBJECTS: usize = 64;
const CREATE_GRAPHICS: usize =
    offset_of!(ID3D12Device_Vtbl, CreateGraphicsPipelineState) / size_of::<usize>();
const CREATE_STREAM: usize =
    offset_of!(ID3D12Device2_Vtbl, CreatePipelineState) / size_of::<usize>();
const CREATE_LIBRARY: usize =
    offset_of!(ID3D12Device1_Vtbl, CreatePipelineLibrary) / size_of::<usize>();
const LOAD_GRAPHICS: usize =
    offset_of!(ID3D12PipelineLibrary_Vtbl, LoadGraphicsPipeline) / size_of::<usize>();
const LOAD_STREAM: usize =
    offset_of!(ID3D12PipelineLibrary1_Vtbl, LoadPipeline) / size_of::<usize>();
pub(super) const SET_PIPELINE: usize =
    offset_of!(ID3D12GraphicsCommandList_Vtbl, SetPipelineState) / size_of::<usize>();

#[derive(Default)]
pub(super) struct PsoRegistry {
    states: RwLock<HashMap<usize, Option<PsoObservation>>>,
    created_graphics: AtomicU64,
    created_stream: AtomicU64,
    stream_parse_unknown: AtomicU64,
    library_loads: AtomicU64,
    released: AtomicU64,
    selected_unknown: AtomicU64,
}

impl PsoRegistry {
    /// Diagnostic-only census of source-filter rejections; never blocks a Draw or changes
    /// production PSO observations. A busy/absent registry is explicitly unknown.
    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    pub(super) fn diagnostic_depth_write(&self, raw: usize) -> Option<bool> {
        self.states
            .try_read()
            .ok()
            .and_then(|states| states.get(&raw).copied().flatten())
            .map(|state| state.depth.write)
    }

    #[cfg(test)]
    pub(super) fn register_test_writer(&self, raw: usize) {
        let mut test = details::stream_default();
        test.dsv_format = 20;
        self.register_observation(
            raw,
            Some(PsoObservation {
                depth: depth_state(1, 1, 20).unwrap(),
                test: Some(test),
            }),
        );
    }
    #[cfg(test)]
    fn register(&self, raw: usize, state: Option<PipelineDepthState>) {
        self.register_observation(raw, state.map(|depth| PsoObservation { depth, test: None }));
    }

    fn register_observation(&self, raw: usize, state: Option<PsoObservation>) {
        if raw != 0
            && let Ok(mut states) = self.states.write()
        {
            // Always replace, including a new unknown PSO reusing this address.
            states.insert(raw, state);
        }
    }

    fn remove(&self, raw: usize) {
        if let Ok(mut states) = self.states.write() {
            states.remove(&raw);
        }
    }

    fn released(&self, raw: usize, remaining: u32) {
        if remaining == 0 {
            self.remove(raw);
            self.released.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn observe_selected(&self, bindings: &mut SelectedDrawBindings) {
        let observation = self
            .states
            .read()
            .ok()
            .and_then(|states| states.get(&bindings.pipeline_state).copied().flatten());
        bindings.pipeline_depth = observation.map(|state| state.depth);
        if let Some(context) = bindings.pass_context.as_mut() {
            context.pipeline = observation.and_then(|state| state.test);
        }
        if bindings.pipeline_depth.is_none() {
            self.selected_unknown.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Different PSO objects are interchangeable for camera evidence only when both have a known,
    /// exactly equal depth/stencil observation. Shader/material identity is irrelevant to the
    /// authored camera CBV, while unknown states remain distinct and force the full Draw path.
    pub(super) fn production_semantically_equal(&self, cached: usize, current: usize) -> bool {
        if cached == current {
            return true;
        }
        let Ok(states) = self.states.read() else {
            return false;
        };
        matches!(
            (states.get(&cached), states.get(&current)),
            (Some(Some(cached)), Some(Some(current))) if cached == current
        )
    }

    pub(super) fn append_counters(&self, output: &mut BTreeMap<String, TimingSnapshot>) {
        for (name, count) in [
            ("pso_created_graphics", &self.created_graphics),
            ("pso_created_stream", &self.created_stream),
            ("pso_stream_parse_unknown", &self.stream_parse_unknown),
            ("pso_library_loads", &self.library_loads),
            ("pso_released", &self.released),
            ("selected_draws_pso_unknown", &self.selected_unknown),
        ] {
            output.insert(
                name.into(),
                TimingSnapshot {
                    calls: count.load(Ordering::Relaxed),
                    ..Default::default()
                },
            );
        }
    }
}

fn depth_state(enable: i32, mask: i32, format: i32) -> Option<PipelineDepthState> {
    if mask != D3D12_DEPTH_WRITE_MASK_ZERO.0 && mask != D3D12_DEPTH_WRITE_MASK_ALL.0 {
        return None;
    }
    Some(PipelineDepthState {
        enable: enable != 0,
        write: enable != 0
            && mask == D3D12_DEPTH_WRITE_MASK_ALL.0
            && format != DXGI_FORMAT_UNKNOWN.0,
    })
}

fn aligned(value: usize, alignment: usize) -> Option<usize> {
    value
        .checked_add(alignment - 1)
        .map(|v| v & !(alignment - 1))
}

fn layout<T>() -> (usize, usize) {
    (size_of::<T>(), align_of::<T>())
}

fn subobject_layout(kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE) -> Option<(usize, usize)> {
    Some(match kind {
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE => layout::<usize>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS => layout::<D3D12_SHADER_BYTECODE>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_STREAM_OUTPUT => layout::<D3D12_STREAM_OUTPUT_DESC>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND => layout::<D3D12_BLEND_DESC>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_NODE_MASK
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_FLAGS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY => layout::<u32>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER => layout::<D3D12_RASTERIZER_DESC>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1 => layout::<D3D12_RASTERIZER_DESC1>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2 => layout::<D3D12_RASTERIZER_DESC2>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL => layout::<D3D12_DEPTH_STENCIL_DESC>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1 => layout::<D3D12_DEPTH_STENCIL_DESC1>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => layout::<D3D12_DEPTH_STENCIL_DESC2>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT => layout::<D3D12_INPUT_LAYOUT_DESC>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS => {
            layout::<D3D12_RT_FORMAT_ARRAY>()
        }
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT => layout::<DXGI_FORMAT>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC => layout::<DXGI_SAMPLE_DESC>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO => layout::<D3D12_CACHED_PIPELINE_STATE>(),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING => {
            layout::<D3D12_VIEW_INSTANCING_DESC>()
        }
        _ => return None,
    })
}

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(
        bytes.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Each enum/payload STRUCT is pointer-aligned; the payload has its own native
/// alignment. Padding the enum alone to pointer width is wrong for u32 payloads.
/// Never dereference shader/input-layout/root-signature pointers inside a stream.
/// https://learn.microsoft.com/en-us/windows/win32/api/d3d12/ns-d3d12-d3d12_pipeline_state_stream_desc
fn parse_stream(bytes: &[u8]) -> Option<PsoObservation> {
    if bytes.is_empty() || bytes.len() > MAX_STREAM_BYTES {
        return None;
    }
    let (mut enable, mut mask, mut format) =
        (1, D3D12_DEPTH_WRITE_MASK_ALL.0, DXGI_FORMAT_UNKNOWN.0);
    let mut test = Some(details::stream_default());
    let mut offset = 0;
    let mut seen = 0u64;
    let mut count = 0;
    while offset < bytes.len() {
        count += 1;
        if count > MAX_SUBOBJECTS {
            return None;
        }
        let kind = D3D12_PIPELINE_STATE_SUBOBJECT_TYPE(i32_at(bytes, offset)?);
        let (size, alignment) = subobject_layout(kind)?;
        let canonical = match kind {
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => {
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL.0
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2 => {
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER.0
            }
            _ => kind.0,
        };
        let bit = 1u64.checked_shl(u32::try_from(canonical).ok()?)?;
        if seen & bit != 0 {
            return None;
        }
        seen |= bit;
        let payload = offset.checked_add(aligned(
            size_of::<D3D12_PIPELINE_STATE_SUBOBJECT_TYPE>(),
            alignment,
        )?)?;
        let end = aligned(payload.checked_add(size)?, align_of::<usize>())?;
        bytes.get(offset..end)?;
        match kind {
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => {
                enable = i32_at(bytes, payload)?;
                mask = i32_at(bytes, payload + 4)?;
                test = details::from_stream(bytes, payload, kind);
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT => {
                format = i32_at(bytes, payload)?
            }
            _ => {}
        }
        offset = end;
    }
    if let Some(test) = test.as_mut() {
        test.dsv_format = format;
    }
    Some(PsoObservation {
        depth: depth_state(enable, mask, format)?,
        test,
    })
}

fn read_i32(address: usize) -> Option<i32> {
    if !readable_mapped_range(address, 4) {
        return None;
    }
    Some(unsafe { std::ptr::read_unaligned(address as *const i32) })
}

fn graphics_state(desc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Option<PsoObservation> {
    let base = desc as usize;
    if base == 0 {
        return None;
    }
    let depth = base.checked_add(offset_of!(
        D3D12_GRAPHICS_PIPELINE_STATE_DESC,
        DepthStencilState
    ))?;
    let format =
        read_i32(base.checked_add(offset_of!(D3D12_GRAPHICS_PIPELINE_STATE_DESC, DSVFormat))?)?;
    let state = depth_state(read_i32(depth)?, read_i32(depth.checked_add(4)?)?, format)?;
    let test = if readable_mapped_range(depth, size_of::<D3D12_DEPTH_STENCIL_DESC>()) {
        let mut test = details::from_desc(unsafe {
            std::ptr::read_unaligned(depth as *const D3D12_DEPTH_STENCIL_DESC)
        });
        test.dsv_format = format;
        Some(test)
    } else {
        None
    };
    Some(PsoObservation { depth: state, test })
}

fn stream_state(desc: *const D3D12_PIPELINE_STATE_STREAM_DESC) -> Option<PsoObservation> {
    if desc.is_null()
        || !readable_mapped_range(desc as usize, size_of::<D3D12_PIPELINE_STATE_STREAM_DESC>())
    {
        return None;
    }
    let desc = unsafe { std::ptr::read_unaligned(desc) };
    if desc.SizeInBytes == 0
        || desc.SizeInBytes > MAX_STREAM_BYTES
        || !(desc.pPipelineStateSubobjectStream as usize).is_multiple_of(align_of::<usize>())
        || !readable_mapped_range(
            desc.pPipelineStateSubobjectStream as usize,
            desc.SizeInBytes,
        )
    {
        return None;
    }
    parse_stream(unsafe {
        std::slice::from_raw_parts(
            desc.pPipelineStateSubobjectStream.cast::<u8>(),
            desc.SizeInBytes,
        )
    })
}

impl D3d12ObservationBackend {
    pub(super) unsafe fn install_pipeline_devices(&self, device: *mut c_void) {
        // Best-effort observations must not change capture admission or the
        // application's HRESULT. Missing interfaces/hooks yield unknown PSOs.
        let _ = unsafe {
            self.install_method(
                device,
                CREATE_GRAPHICS,
                HookMethod::CreateGraphicsPipelineState,
                hook_create_graphics as *const () as usize,
            )
        };
        if let Some(unknown) = unsafe { IUnknown::from_raw_borrowed(&device) } {
            if let Ok(device1) = unknown.cast::<ID3D12Device1>() {
                let _ = unsafe {
                    self.install_method(
                        device1.as_raw(),
                        CREATE_LIBRARY,
                        HookMethod::CreatePipelineLibrary,
                        hook_create_library as *const () as usize,
                    )
                };
            }
            if let Ok(device2) = unknown.cast::<ID3D12Device2>() {
                let _ = unsafe {
                    self.install_method(
                        device2.as_raw(),
                        CREATE_STREAM,
                        HookMethod::CreatePipelineState,
                        hook_create_stream as *const () as usize,
                    )
                };
            }
        }
    }

    unsafe fn register_created_pso(&self, out: *mut *mut c_void, state: Option<PsoObservation>) {
        if out.is_null() {
            return;
        }
        let object = unsafe { *out };
        let Some(unknown) = (unsafe { IUnknown::from_raw_borrowed(&object) }) else {
            return;
        };
        let Ok(pso) = unknown.cast::<ID3D12PipelineState>() else {
            return;
        };
        let raw = pso.as_raw();
        if raw != object {
            return;
        }
        // Do not publish any state unless its lifetime can be observed.
        if unsafe {
            self.install_method(
                raw,
                iunknown_release_index(),
                HookMethod::ReleasePipelineState,
                hook_release_pso as *const () as usize,
            )
        }
        .is_ok()
        {
            self.pso_registry.register_observation(raw as usize, state);
        } else {
            self.pso_registry.remove(raw as usize);
        }
    }

    unsafe fn install_pipeline_library(&self, out: *mut *mut c_void) {
        if out.is_null() {
            return;
        }
        let object = unsafe { *out };
        let Some(unknown) = (unsafe { IUnknown::from_raw_borrowed(&object) }) else {
            return;
        };
        if let Ok(library) = unknown.cast::<ID3D12PipelineLibrary>() {
            let _ = unsafe {
                self.install_method(
                    library.as_raw(),
                    LOAD_GRAPHICS,
                    HookMethod::LoadGraphicsPipeline,
                    hook_load_graphics as *const () as usize,
                )
            };
        }
        if let Ok(library) = unknown.cast::<ID3D12PipelineLibrary1>() {
            let _ = unsafe {
                self.install_method(
                    library.as_raw(),
                    LOAD_STREAM,
                    HookMethod::LoadPipeline,
                    hook_load_stream as *const () as usize,
                )
            };
        }
    }
}

type GraphicsFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type StreamFn = unsafe extern "system" fn(
    *mut c_void,
    *const D3D12_PIPELINE_STATE_STREAM_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type LibraryFn = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    usize,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type LoadGraphicsFn = unsafe extern "system" fn(
    *mut c_void,
    PCWSTR,
    *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type LoadStreamFn = unsafe extern "system" fn(
    *mut c_void,
    PCWSTR,
    *const D3D12_PIPELINE_STATE_STREAM_DESC,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;
type SetFn = unsafe extern "system" fn(*mut c_void, *mut c_void);
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

unsafe extern "system" fn hook_create_graphics(
    this: *mut c_void,
    desc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(b) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = b.original_target(
        this,
        CREATE_GRAPHICS,
        HookMethod::CreateGraphicsPipelineState,
    ) else {
        b.block("CreateGraphicsPipelineState original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: GraphicsFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, desc, riid, out) };
    if result.is_ok() && !out.is_null() && !unsafe { *out }.is_null() {
        b.pso_registry
            .created_graphics
            .fetch_add(1, Ordering::Relaxed);
        unsafe {
            b.register_created_pso(out, graphics_state(desc));
        }
    }
    result
}

unsafe extern "system" fn hook_create_stream(
    this: *mut c_void,
    desc: *const D3D12_PIPELINE_STATE_STREAM_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(b) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = b.original_target(this, CREATE_STREAM, HookMethod::CreatePipelineState)
    else {
        b.block("CreatePipelineState original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: StreamFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, desc, riid, out) };
    if result.is_ok() && !out.is_null() && !unsafe { *out }.is_null() {
        b.pso_registry
            .created_stream
            .fetch_add(1, Ordering::Relaxed);
        let state = stream_state(desc);
        if state.is_none() {
            b.pso_registry
                .stream_parse_unknown
                .fetch_add(1, Ordering::Relaxed);
        }
        unsafe {
            b.register_created_pso(out, state);
        }
    }
    result
}

unsafe extern "system" fn hook_create_library(
    this: *mut c_void,
    blob: *const c_void,
    size: usize,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(b) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = b.original_target(this, CREATE_LIBRARY, HookMethod::CreatePipelineLibrary)
    else {
        b.block("CreatePipelineLibrary original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: LibraryFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, blob, size, riid, out) };
    if result.is_ok() {
        unsafe {
            b.install_pipeline_library(out);
        }
    }
    result
}

unsafe extern "system" fn hook_load_graphics(
    this: *mut c_void,
    name: PCWSTR,
    desc: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(b) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = b.original_target(this, LOAD_GRAPHICS, HookMethod::LoadGraphicsPipeline)
    else {
        b.block("LoadGraphicsPipeline original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: LoadGraphicsFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, name, desc, riid, out) };
    if result.is_ok() {
        b.pso_registry.library_loads.fetch_add(1, Ordering::Relaxed);
        unsafe {
            b.register_created_pso(out, graphics_state(desc));
        }
    }
    result
}

unsafe extern "system" fn hook_load_stream(
    this: *mut c_void,
    name: PCWSTR,
    desc: *const D3D12_PIPELINE_STATE_STREAM_DESC,
    riid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(b) = backend() else {
        return HRESULT(0x80004005_u32 as i32);
    };
    let Some(target) = b.original_target(this, LOAD_STREAM, HookMethod::LoadPipeline) else {
        b.block("LoadPipeline original target missing");
        return HRESULT(0x80004005_u32 as i32);
    };
    let original: LoadStreamFn = unsafe { std::mem::transmute(target) };
    let result = unsafe { original(this, name, desc, riid, out) };
    if result.is_ok() && !out.is_null() && !unsafe { *out }.is_null() {
        b.pso_registry.library_loads.fetch_add(1, Ordering::Relaxed);
        let state = stream_state(desc);
        if state.is_none() {
            b.pso_registry
                .stream_parse_unknown
                .fetch_add(1, Ordering::Relaxed);
        }
        unsafe {
            b.register_created_pso(out, state);
        }
    }
    result
}

unsafe extern "system" fn hook_release_pso(this: *mut c_void) -> u32 {
    let Some(b) = backend() else {
        return 0;
    };
    let Some(target) = b.original_target(
        this,
        iunknown_release_index(),
        HookMethod::ReleasePipelineState,
    ) else {
        b.block("PSO Release original target missing");
        return 0;
    };
    let original: ReleaseFn = unsafe { std::mem::transmute(target) };
    let remaining = unsafe { original(this) };
    // `this` may already be freed. Only use its numeric key after Release.
    b.pso_registry.released(this as usize, remaining);
    remaining
}

pub(super) unsafe extern "system" fn hook_set_pipeline_state(
    this: *mut c_void,
    state: *mut c_void,
) {
    let Some(b) = backend() else {
        return;
    };
    let Some(target) = b.original_target(this, SET_PIPELINE, HookMethod::SetPipelineState) else {
        b.block("SetPipelineState original target missing");
        return;
    };
    let original: SetFn = unsafe { std::mem::transmute(target) };
    unsafe {
        set_pipeline_after_original(b, original, this, state);
    }
}

unsafe fn set_pipeline_after_original(
    b: &D3d12ObservationBackend,
    original: SetFn,
    this: *mut c_void,
    state: *mut c_void,
) {
    unsafe {
        original(this, state);
    }
    b.hot_lists
        .bind_pipeline_state(this as usize, state as usize);
}

#[allow(dead_code)]
fn assert_abi(
    d: &ID3D12Device_Vtbl,
    d1: &ID3D12Device1_Vtbl,
    d2: &ID3D12Device2_Vtbl,
    l: &ID3D12PipelineLibrary_Vtbl,
    l1: &ID3D12PipelineLibrary1_Vtbl,
    list: &ID3D12GraphicsCommandList_Vtbl,
) {
    let _: GraphicsFn = d.CreateGraphicsPipelineState;
    let _: StreamFn = d2.CreatePipelineState;
    let _: LibraryFn = d1.CreatePipelineLibrary;
    let _: LoadGraphicsFn = l.LoadGraphicsPipeline;
    let _: LoadStreamFn = l1.LoadPipeline;
    let _: SetFn = list.SetPipelineState;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12_abi::{DescriptorHandle, DsvIdentity, QueueId, ResourceId};
    use crate::d3d12_render_camera::RootBindingSource;
    use std::cell::Cell;

    #[repr(C)]
    #[cfg_attr(target_pointer_width = "64", repr(align(8)))]
    #[cfg_attr(target_pointer_width = "32", repr(align(4)))]
    struct StreamElement<T> {
        kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE,
        value: T,
    }

    // Build initialized bytes using the compiler's C struct layout, independently
    // of the parser's alignment arithmetic. Do not copy uninitialized padding.
    pub(super) fn append<T>(
        bytes: &mut Vec<u8>,
        kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE,
        prefix: &[i32],
    ) {
        let start = bytes.len();
        bytes.resize(start + size_of::<StreamElement<T>>(), 0);
        bytes[start..start + 4].copy_from_slice(&kind.0.to_le_bytes());
        for (i, value) in prefix.iter().enumerate() {
            let at = start + offset_of!(StreamElement<T>, value) + i * 4;
            bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
    }

    fn with_format() -> Vec<u8> {
        let mut bytes = Vec::new();
        append::<DXGI_FORMAT>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
            &[40],
        );
        bytes
    }

    #[test]
    fn pso_stream_alignment_and_default_depth_state() {
        let mut bytes = Vec::new();
        append::<u32>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
            &[3],
        );
        append::<usize>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
            &[0x1234],
        );
        append::<D3D12_SHADER_BYTECODE>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS,
            &[0x5678],
        );
        bytes.extend(with_format());
        assert_eq!(
            parse_stream(&bytes).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: true,
                write: true
            })
        );
        assert_eq!(offset_of!(StreamElement<DXGI_FORMAT>, value), 4);
        assert_eq!(offset_of!(StreamElement<usize>, value), size_of::<usize>());
        assert_eq!(
            parse_stream(&with_format()).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: true,
                write: true
            })
        );
    }

    #[test]
    fn pso_stream_all_three_depth_stencil_variants_and_write_conditions() {
        for variant in 0..3 {
            for (enable, mask, write) in [(1, 1, true), (1, 0, false), (0, 1, false)] {
                let mut bytes = with_format();
                match variant {
                    0 => append::<D3D12_DEPTH_STENCIL_DESC>(
                        &mut bytes,
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
                        &[enable, mask],
                    ),
                    1 => append::<D3D12_DEPTH_STENCIL_DESC1>(
                        &mut bytes,
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1,
                        &[enable, mask],
                    ),
                    _ => append::<D3D12_DEPTH_STENCIL_DESC2>(
                        &mut bytes,
                        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2,
                        &[enable, mask],
                    ),
                }
                assert_eq!(
                    parse_stream(&bytes).map(|state| state.depth),
                    Some(PipelineDepthState {
                        enable: enable != 0,
                        write
                    })
                );
            }
        }
        assert_eq!(offset_of!(D3D12_DEPTH_STENCIL_DESC, DepthWriteMask), 4);
        assert_eq!(offset_of!(D3D12_DEPTH_STENCIL_DESC1, DepthWriteMask), 4);
        assert_eq!(offset_of!(D3D12_DEPTH_STENCIL_DESC2, DepthWriteMask), 4);
    }

    #[test]
    fn pso_stream_missing_or_unknown_dsv_format_never_writes_depth() {
        let mut bytes = Vec::new();
        append::<D3D12_DEPTH_STENCIL_DESC>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
            &[1, 1],
        );
        assert_eq!(
            parse_stream(&bytes).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: true,
                write: false
            })
        );
        append::<DXGI_FORMAT>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
            &[0],
        );
        assert_eq!(
            parse_stream(&bytes).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: true,
                write: false
            })
        );
    }

    #[test]
    fn pso_stream_unknown_duplicate_truncated_and_oversize_are_unknown() {
        let mut unknown = with_format();
        unknown.extend(999i32.to_le_bytes());
        unknown.extend([0; 4]);
        assert_eq!(parse_stream(&unknown), None);
        assert_eq!(parse_stream(&[]), None);
        assert_eq!(parse_stream(&vec![0; MAX_STREAM_BYTES + 1]), None);
        let mut bytes = Vec::new();
        append::<D3D12_DEPTH_STENCIL_DESC2>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2,
            &[1, 1],
        );
        for len in 0..bytes.len() {
            assert_eq!(parse_stream(&bytes[..len]), None, "length={len}");
        }
        append::<D3D12_DEPTH_STENCIL_DESC>(
            &mut bytes,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
            &[1, 1],
        );
        assert_eq!(parse_stream(&bytes), None);
        let mut invalid_mask = with_format();
        append::<D3D12_DEPTH_STENCIL_DESC>(
            &mut invalid_mask,
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
            &[1, 99],
        );
        assert_eq!(parse_stream(&invalid_mask), None);
        assert_eq!(stream_state(std::ptr::null()), None);
        assert_eq!(graphics_state(std::ptr::null()), None);
        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: MAX_STREAM_BYTES + 1,
            pPipelineStateSubobjectStream: std::ptr::null_mut(),
        };
        assert_eq!(stream_state(&desc), None);
    }

    #[test]
    fn pso_graphics_descriptor_state_is_read_without_owning_embedded_com_fields() {
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC::default();
        desc.DepthStencilState.DepthEnable = true.into();
        desc.DepthStencilState.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ALL;
        desc.DSVFormat = DXGI_FORMAT(40);
        assert_eq!(
            graphics_state(&desc).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: true,
                write: true
            })
        );
        desc.DepthStencilState.DepthWriteMask = D3D12_DEPTH_WRITE_MASK_ZERO;
        assert_eq!(
            graphics_state(&desc).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: true,
                write: false
            })
        );
        desc.DepthStencilState.DepthEnable = false.into();
        assert_eq!(
            graphics_state(&desc).map(|state| state.depth),
            Some(PipelineDepthState {
                enable: false,
                write: false
            })
        );
    }

    #[test]
    fn pso_release_and_unknown_recreation_cannot_inherit_previous_state() {
        let backend = D3d12ObservationBackend::new();
        backend.hot_lists.register(0x2000, 9);
        backend.hot_lists.set_pipeline_hook_available(0x2000, true);
        backend.hot_lists.bind_selected_depth(0x2000, 7);
        backend.hot_lists.bind_pipeline_state(0x2000, 0x8000);
        backend.pso_registry.register(
            0x8000,
            Some(PipelineDepthState {
                enable: true,
                write: true,
            }),
        );
        let mut draw = backend.hot_lists.selected_draw(0x2000, 7).unwrap();
        backend.pso_registry.released(0x8000, 1);
        backend.pso_registry.observe_selected(&mut draw);
        assert!(draw.pipeline_depth.unwrap().write);
        backend.pso_registry.released(0x8000, 0);
        backend.pso_registry.observe_selected(&mut draw);
        assert_eq!(draw.pipeline_depth, None);
        backend.pso_registry.register(0x8000, None);
        backend.pso_registry.observe_selected(&mut draw);
        assert_eq!(draw.pipeline_depth, None);
        backend.pso_registry.register(
            0x8000,
            Some(PipelineDepthState {
                enable: false,
                write: false,
            }),
        );
        backend.pso_registry.observe_selected(&mut draw);
        assert_eq!(
            draw.pipeline_depth,
            Some(PipelineDepthState {
                enable: false,
                write: false
            })
        );
        let timings = backend.render_camera_snapshot().hook_cpu_timing;
        assert_eq!(timings["pso_released"].calls, 1);
        assert_eq!(timings["selected_draws_pso_unknown"].calls, 2);
        for name in [
            "pso_created_graphics",
            "pso_created_stream",
            "pso_stream_parse_unknown",
            "pso_library_loads",
            "pso_released",
            "selected_draws_pso_unknown",
        ] {
            assert_eq!((timings[name].total_us, timings[name].max_us), (0, 0));
        }
    }

    #[test]
    fn pso_missing_set_hook_keeps_initial_binding_unknown_and_release_clears_coverage() {
        let backend = D3d12ObservationBackend::new();
        backend.hot_lists.register(0x2000, 9);
        backend.pso_registry.register(
            0x8000,
            Some(PipelineDepthState {
                enable: true,
                write: true,
            }),
        );
        backend.hot_lists.reset_with_pipeline(0x2000, 0x8000);
        backend.hot_lists.bind_selected_depth(0x2000, 7);
        let mut draw = backend.hot_lists.selected_draw(0x2000, 7).unwrap();
        assert_eq!(draw.pipeline_state, 0);
        backend.pso_registry.observe_selected(&mut draw);
        assert_eq!(draw.pipeline_depth, None);
        backend.hot_lists.set_pipeline_hook_available(0x2000, true);
        backend.hot_lists.reset_with_pipeline(0x2000, 0x8000);
        backend.hot_lists.bind_selected_depth(0x2000, 7);
        let mut draw = backend.hot_lists.selected_draw(0x2000, 7).unwrap();
        backend.pso_registry.observe_selected(&mut draw);
        assert!(draw.pipeline_depth.unwrap().write);
        backend.hot_lists.release(0x2000);
        backend.hot_lists.register(0x2000, 10);
        backend.hot_lists.reset_with_pipeline(0x2000, 0x8000);
        backend.hot_lists.bind_selected_depth(0x2000, 7);
        let mut draw = backend.hot_lists.selected_draw(0x2000, 7).unwrap();
        backend.pso_registry.observe_selected(&mut draw);
        assert_eq!(draw.pipeline_depth, None);
    }

    struct SetCall {
        backend: std::sync::Arc<D3d12ObservationBackend>,
        called: Cell<bool>,
    }

    unsafe extern "system" fn fake_set(this: *mut c_void, _: *mut c_void) {
        let call = unsafe { &*this.cast::<SetCall>() };
        assert_eq!(
            call.backend
                .hot_lists
                .find(this as usize)
                .unwrap()
                .pipeline_state
                .load(Ordering::Acquire),
            0
        );
        call.called.set(true);
    }

    #[test]
    fn pso_set_and_reset_initial_state_reach_draw_observation_without_changing_admission() {
        for reset in [false, true] {
            for state in [
                Some(PipelineDepthState {
                    enable: true,
                    write: true,
                }),
                Some(PipelineDepthState {
                    enable: true,
                    write: false,
                }),
                None,
            ] {
                let b = D3d12ObservationBackend::new();
                let mut call = SetCall {
                    backend: std::sync::Arc::clone(&b),
                    called: Cell::new(false),
                };
                let raw = (&mut call as *mut SetCall).cast::<c_void>();
                b.hot_lists.register(raw as usize, 9);
                b.hot_lists.set_pipeline_hook_available(raw as usize, true);
                b.pso_registry.register_observation(
                    0x8000,
                    state.map(|depth| PsoObservation {
                        depth,
                        test: Some(details::stream_default()),
                    }),
                );
                let mut tracker = b.render_camera.lock().unwrap();
                tracker.register_list(raw as usize, 9);
                if reset {
                    b.hot_lists.reset_with_pipeline(raw as usize, 0x8000);
                    tracker.reset_list(raw as usize);
                } else {
                    // The callback observes the old binding, proving original-first.
                    unsafe {
                        set_pipeline_after_original(&b, fake_set, raw, 0x8000usize as *mut c_void);
                    }
                    assert!(call.called.get());
                }
                tracker.register_buffer(0x1000, 0x10_0000, 512);
                tracker.map_buffer(0x1000, 0x20_0000);
                tracker.arm_selected_depth(DsvIdentity {
                    handle: DescriptorHandle(4),
                    descriptor_generation: 2,
                    resource: ResourceId(7),
                    resource_epoch: 3,
                });
                b.hot_lists.bind_selected_depth(raw as usize, 7);
                b.hot_lists
                    .bind_root(raw as usize, 4, super::super::ROOT_DIRECT_CBV, 0x10_0000);
                let mut draw = b.hot_lists.selected_draw(raw as usize, 7).unwrap();
                assert_eq!(draw.pipeline_state, 0x8000);
                assert_eq!(draw.roots[0].source, RootBindingSource::Direct);
                draw.pass_context = Some(tracker.draw_pass_context(raw as usize, Some(0)));
                b.pso_registry.observe_selected(&mut draw);
                let frozen_pass = draw.pass_context;
                let x = 1.0 / (75.0_f32.to_radians() * 0.5).tan();
                let matrices: [[f32; 4]; 8] = [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                    [1.0, 2.0, 3.0, 1.0],
                    [x, 0.0, 0.0, 0.0],
                    [0.0, x * 16.0 / 9.0, 0.0, 0.0],
                    [0.0, 0.0, 0.0001, 1.0],
                    [0.0, 0.0, 0.1, 0.0],
                ];
                let mut bytes: Vec<_> = matrices
                    .into_iter()
                    .flatten()
                    .flat_map(f32::to_le_bytes)
                    .collect();
                bytes.resize(512, 0);
                tracker.observe_selected_draw(raw as usize, &draw, |_, _, size| {
                    Some(bytes[..size].to_vec())
                });
                // A later bind/release must not replace the already recorded draw state.
                b.hot_lists.bind_pipeline_state(raw as usize, 0);
                b.pso_registry.released(0x8000, 0);
                tracker.execute(QueueId(5), &[raw as usize]);
                tracker.mark_depth_copy_submitted(QueueId(5), 44);
                tracker.present(QueueId(5), 44, 1234);
                let snapshot = tracker.snapshot();
                assert_eq!(snapshot.observations.len(), 1);
                let observation = &snapshot.observations[0];
                assert_eq!(observation.pass_context, frozen_pass);
                assert_eq!(
                    observation.pass_context.unwrap().pipeline,
                    state.map(|_| details::stream_default())
                );
                assert_eq!(observation.pipeline_depth_enable, state.map(|s| s.enable));
                assert_eq!(observation.pipeline_depth_write, state.map(|s| s.write));
                assert!(
                    observation.copy_before_clear_submitted
                        && observation.selected_present_identity_passed
                );
                let json = serde_json::to_value(observation).unwrap();
                assert!(json.get("pipeline_state").is_none());
                assert_eq!(
                    json["pipeline_depth_enable"],
                    serde_json::to_value(state.map(|s| s.enable)).unwrap()
                );
                b.hot_lists.reset_with_pipeline(raw as usize, 0);
                b.hot_lists.bind_selected_depth(raw as usize, 7);
                assert_eq!(
                    b.hot_lists
                        .selected_draw(raw as usize, 7)
                        .unwrap()
                        .pipeline_state,
                    0
                );
            }
        }
    }
}

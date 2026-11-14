//! COM vtable hook：把接口虚表里的一个函数指针换成我们的，并保留原指针。
//!
//! # 为什么不用 MinHook 之类的内联 hook 库
//!
//! 我们要 hook 的全部是 **COM 接口方法**（`IDXGISwapChain::Present`、
//! `ID3D11DeviceContext::OMSetRenderTargets` …）。COM 对象的第一个字段就是 vtable 指针，
//! 换掉表里的一项即可——不需要改写目标函数的头几个字节、不需要重定位指令、
//! 不需要处理指令边界。代码量差一个数量级，出错面也小得多。
//!
//! 代价是：**同一个进程里若有另一个组件也 hook 了同一项**（ReShade、Steam Overlay、
//! RTSS 都会），槽位可能在运行中再次变化。不能仅凭本地还留着一个 `VtableHook` 就认定
//! hook 仍然生效；调用方必须审计槽位。只有槽位精确恢复成安装时保存的 `original` 才能
//! 安全重挂；未知目标可能已经把我们的 replacement 当成它的下游，盲目套回去会递归。
//!
//! # 安全性边界
//!
//! 本模块是整个 crate 唯一必须 `unsafe` 的地方。它只做三件事：
//! 读 vtable 指针、`VirtualProtect` 改页保护、写一个 `usize`。
//! 所有对「这个指针是不是真的指向一个 COM 对象」的保证都由调用方负责。

#[cfg(windows)]
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
#[cfg(all(windows, test))]
use windows::Win32::Graphics::Direct3D11::ID3D11Multithread_Vtbl;
#[cfg(windows)]
use windows::Win32::Graphics::Direct3D11::{
    ID3D11DeviceContext_Vtbl, ID3D11DeviceContext1_Vtbl, ID3D11DeviceContext2_Vtbl,
    ID3D11DeviceContext3_Vtbl, ID3D11DeviceContext4_Vtbl,
};
#[cfg(windows)]
use windows::Win32::System::Memory::{
    PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, VirtualProtect,
};

#[cfg(windows)]
pub const D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D11DeviceContext_Vtbl>() / core::mem::size_of::<usize>();
#[cfg(windows)]
pub const D3D11_DEVICE_CONTEXT1_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D11DeviceContext1_Vtbl>() / core::mem::size_of::<usize>();
#[cfg(windows)]
pub const D3D11_DEVICE_CONTEXT2_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D11DeviceContext2_Vtbl>() / core::mem::size_of::<usize>();
#[cfg(windows)]
pub const D3D11_DEVICE_CONTEXT3_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D11DeviceContext3_Vtbl>() / core::mem::size_of::<usize>();
#[cfg(windows)]
pub const D3D11_DEVICE_CONTEXT4_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D11DeviceContext4_Vtbl>() / core::mem::size_of::<usize>();

/// 一个已安装的 vtable hook，`Drop` 时自动还原。
///
/// **`Drop` 里还原是刻意的**：游戏卸载我们的 DLL、或 probe 主动停止时，
/// 若不还原，游戏后续调用会跳进已卸载的内存——直接崩溃，而且崩在游戏的调用栈上，
/// 看起来像是游戏自己的 bug。
#[cfg(windows)]
pub struct VtableHook {
    slot: *mut usize,
    original: usize,
    replacement: usize,
}

/// 当前对象对应槽位相对一个已安装 hook 的状态。
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtableSlotState {
    Installed,
    RestoredOriginal,
    ReplacedUnknown {
        current: usize,
    },
    DifferentSlot {
        expected_slot: usize,
        actual_slot: usize,
        current: usize,
    },
}

/// COM 对象当前使用哪一张 vtable。
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VtableOwnerState {
    Active,
    RestoredOriginal,
    ReplacedUnknown { current: usize },
}

/// 为单个 COM 对象持有一张完整的 shadow vtable。
///
/// D3D11 immediate context 的实现可把分发表内嵌在对象旁边，并在 Present/驱动调度时
/// 重铺其中一部分槽位。逐槽修改那张运行时表会被反复擦掉。shadow 复制完整 ABI 表后
/// 只交换对象首指针；运行时继续更新 backing table，不再破坏我们的调用入口。
#[cfg(windows)]
pub struct VtableShadow {
    object_vtable: *mut *mut usize,
    original_vtable: *mut usize,
    shadow: Box<[usize]>,
}

#[cfg(windows)]
impl VtableShadow {
    /// 复制 `object` 的完整 vtable，并原子地让该对象改用 shadow。
    ///
    /// # Safety
    ///
    /// - `object` 必须是有效 COM 对象，且首字段为对至少 `entry_count` 项的 vtable 指针；
    /// - 返回值必须存活到该对象不再调用这些方法为止；
    /// - 调用方必须保证同一对象的 shadow 安装串行执行。
    pub unsafe fn capture_and_activate(
        object: *mut core::ffi::c_void,
        entry_count: usize,
    ) -> Result<Self, String> {
        if object.is_null() || entry_count == 0 {
            return Err("shadow vtable object/entry_count invalid".into());
        }
        let object_vtable = object.cast::<*mut usize>();
        if !(object_vtable as usize).is_multiple_of(core::mem::align_of::<AtomicPtr<usize>>()) {
            return Err("shadow vtable owner pointer is not atomically aligned".into());
        }
        let original_vtable = unsafe { *object_vtable };
        if original_vtable.is_null() {
            return Err("shadow vtable original pointer is null".into());
        }
        if !(original_vtable as usize).is_multiple_of(core::mem::align_of::<AtomicUsize>()) {
            return Err("shadow vtable backing pointer is not atomically aligned".into());
        }
        let shadow = unsafe { core::slice::from_raw_parts(original_vtable, entry_count) }
            .to_vec()
            .into_boxed_slice();
        let value = Self {
            object_vtable,
            original_vtable,
            shadow,
        };
        unsafe { value.rearm_if_restored() }?;
        Ok(value)
    }

    #[must_use]
    pub fn owner_state(&self) -> VtableOwnerState {
        let current = unsafe { AtomicPtr::from_ptr(self.object_vtable) }.load(Ordering::Acquire);
        if current == self.shadow.as_ptr().cast_mut() {
            VtableOwnerState::Active
        } else if current == self.original_vtable {
            VtableOwnerState::RestoredOriginal
        } else {
            VtableOwnerState::ReplacedUnknown {
                current: current as usize,
            }
        }
    }

    /// 对象首指针只有在精确恢复为安装时的 backing vtable 时才重新指向 shadow。
    ///
    /// # Safety
    ///
    /// 创建时传入的 COM 对象必须仍然有效。
    pub unsafe fn rearm_if_restored(&self) -> Result<bool, String> {
        let object_vtable = unsafe { AtomicPtr::from_ptr(self.object_vtable) };
        match object_vtable.compare_exchange(
            self.original_vtable,
            self.shadow.as_ptr().cast_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(true),
            Err(current) if current == self.shadow.as_ptr().cast_mut() => Ok(false),
            Err(current) => Err(format!(
                "refuse to replace unknown object vtable: original={:#x} shadow={:#x} current={:#x}",
                self.original_vtable as usize,
                self.shadow.as_ptr() as usize,
                current as usize,
            )),
        }
    }

    #[must_use]
    pub fn slot_state(&self, index: usize, original: usize, replacement: usize) -> VtableSlotState {
        let slot = unsafe { self.shadow.as_ptr().add(index).cast_mut() };
        let current = unsafe { AtomicUsize::from_ptr(slot) }.load(Ordering::Acquire);
        classify_slot_state(slot as usize, slot as usize, original, replacement, current)
    }

    /// shadow 槽只在精确恢复成该 hook 保存的 original 时修复，不接未知链。
    pub fn rearm_slot_if_restored(
        &self,
        index: usize,
        original: usize,
        replacement: usize,
    ) -> Result<bool, String> {
        let slot = unsafe { self.shadow.as_ptr().add(index).cast_mut() };
        let atomic = unsafe { AtomicUsize::from_ptr(slot) };
        match atomic.compare_exchange(original, replacement, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => Ok(true),
            Err(current) if current == replacement => Ok(false),
            Err(current) => Err(format!(
                "refuse to replace unknown shadow slot[{index}]: original={original:#x} replacement={replacement:#x} current={current:#x}"
            )),
        }
    }

    #[must_use]
    pub fn shadow_vtable_address(&self) -> usize {
        self.shadow.as_ptr() as usize
    }

    #[must_use]
    pub fn backing_vtable_address(&self) -> usize {
        self.original_vtable as usize
    }

    #[must_use]
    pub fn backing_target(&self, index: usize) -> usize {
        unsafe { AtomicUsize::from_ptr(self.original_vtable.add(index)) }.load(Ordering::Acquire)
    }
}

#[cfg(windows)]
impl Drop for VtableShadow {
    fn drop(&mut self) {
        let object_vtable = unsafe { AtomicPtr::from_ptr(self.object_vtable) };
        let _ = object_vtable.compare_exchange(
            self.shadow.as_ptr().cast_mut(),
            self.original_vtable,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

// SAFETY: vtable 与对象地址在 probe 生命周期内常驻；所有安装/修复由调用方串行，
// 运行期只做对齐的指针宽度原子读取或交换。
#[cfg(windows)]
unsafe impl Send for VtableShadow {}
#[cfg(windows)]
unsafe impl Sync for VtableShadow {}

#[cfg(windows)]
impl VtableHook {
    /// 把 `object` 的 vtable 第 `index` 项换成 `replacement`。
    ///
    /// # Safety
    ///
    /// - `object` 必须是一个有效的、布局以 vtable 指针开头的 COM 对象；
    /// - `index` 必须在该接口的 vtable 范围内；
    /// - `replacement` 必须与被替换方法的调用约定和签名**完全一致**；
    /// - 返回的 `VtableHook` 必须比 `object` 活得短。
    pub unsafe fn install(
        object: *mut core::ffi::c_void,
        index: usize,
        replacement: usize,
    ) -> Result<Self, windows::core::Error> {
        unsafe { Self::install_impl(object, index, replacement, None) }
    }

    /// Install only after publishing the original target to the atomic read by the replacement
    /// trampoline. A protected multi-threaded D3D11 context can otherwise enter the replacement
    /// while its forwarding target is still zero.
    ///
    /// # Safety
    ///
    /// Same requirements as [`Self::install`]. `original_sink` must be the exact atomic read by
    /// `replacement`, and callers must not reuse it for another live hook.
    pub unsafe fn install_with_published_original(
        object: *mut core::ffi::c_void,
        index: usize,
        replacement: usize,
        original_sink: &AtomicUsize,
    ) -> Result<Self, windows::core::Error> {
        unsafe { Self::install_impl(object, index, replacement, Some(original_sink)) }
    }

    unsafe fn install_impl(
        object: *mut core::ffi::c_void,
        index: usize,
        replacement: usize,
        original_sink: Option<&AtomicUsize>,
    ) -> Result<Self, windows::core::Error> {
        let vtable = unsafe { *(object as *mut *mut usize) };
        let slot = unsafe { vtable.add(index) };

        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtect(
                slot as *const core::ffi::c_void,
                core::mem::size_of::<usize>(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )?;
        }
        let original = unsafe { *slot };
        if let Some(original_sink) = original_sink {
            original_sink.store(original, Ordering::SeqCst);
        }
        unsafe { *slot = replacement };
        let mut back = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            let _ = VirtualProtect(
                slot as *const core::ffi::c_void,
                core::mem::size_of::<usize>(),
                old,
                &mut back,
            );
        }

        Ok(Self {
            slot,
            original,
            replacement,
        })
    }

    /// 原函数指针。hook 函数**必须**在做完自己的事之后调用它。
    #[must_use]
    pub fn original(&self) -> usize {
        self.original
    }

    #[must_use]
    pub fn replacement(&self) -> usize {
        self.replacement
    }

    #[must_use]
    pub fn slot_address(&self) -> usize {
        self.slot as usize
    }

    /// 对照当前对象重新读取 vtable 和目标槽位，不能拿安装时的句柄存在性冒充入口存活。
    ///
    /// # Safety
    ///
    /// `object` 与 `index` 必须仍指向安装时同类的有效 COM 接口及其合法槽位。
    pub unsafe fn state(&self, object: *mut core::ffi::c_void, index: usize) -> VtableSlotState {
        let vtable = unsafe { *(object as *mut *mut usize) };
        let actual_slot = unsafe { vtable.add(index) };
        let current = unsafe { *actual_slot };
        classify_slot_state(
            self.slot as usize,
            actual_slot as usize,
            self.original,
            self.replacement,
            current,
        )
    }

    /// 槽位只在“同一个槽、且精确恢复成保存的 original”时重挂。
    ///
    /// 返回 `Ok(true)` 表示本次完成重挂，`Ok(false)` 表示本来就已安装。未知目标和
    /// vtable/槽位漂移一律拒绝，调用方据此 fail-closed。
    ///
    /// # Safety
    ///
    /// 与 [`Self::state`] 相同；此外调用方须保证 hook 生命周期内对象有效。
    pub unsafe fn rearm_if_restored(
        &self,
        object: *mut core::ffi::c_void,
        index: usize,
    ) -> Result<bool, String> {
        match unsafe { self.state(object, index) } {
            VtableSlotState::Installed => return Ok(false),
            VtableSlotState::RestoredOriginal => {}
            state => return Err(format!("拒绝重挂非 original 槽位: {state:?}")),
        }

        let mut old = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            VirtualProtect(
                self.slot as *const core::ffi::c_void,
                core::mem::size_of::<usize>(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
            .map_err(|error| format!("重挂前修改 vtable 页保护失败: {error}"))?;
        }
        let result = unsafe {
            if *self.slot == self.original {
                *self.slot = self.replacement;
                Ok(true)
            } else if *self.slot == self.replacement {
                Ok(false)
            } else {
                Err(format!("重挂竞争时槽位变成未知目标 {:#x}", *self.slot))
            }
        };
        let mut back = PAGE_PROTECTION_FLAGS(0);
        unsafe {
            let _ = VirtualProtect(
                self.slot as *const core::ffi::c_void,
                core::mem::size_of::<usize>(),
                old,
                &mut back,
            );
        }
        result
    }
}

#[cfg(windows)]
fn classify_slot_state(
    expected_slot: usize,
    actual_slot: usize,
    original: usize,
    replacement: usize,
    current: usize,
) -> VtableSlotState {
    if expected_slot != actual_slot {
        VtableSlotState::DifferentSlot {
            expected_slot,
            actual_slot,
            current,
        }
    } else if current == replacement {
        VtableSlotState::Installed
    } else if current == original {
        VtableSlotState::RestoredOriginal
    } else {
        VtableSlotState::ReplacedUnknown { current }
    }
}

#[cfg(windows)]
impl Drop for VtableHook {
    fn drop(&mut self) {
        unsafe {
            let mut old = PAGE_PROTECTION_FLAGS(0);
            if VirtualProtect(
                self.slot as *const core::ffi::c_void,
                core::mem::size_of::<usize>(),
                PAGE_EXECUTE_READWRITE,
                &mut old,
            )
            .is_ok()
            {
                // 另一个组件若在我们之后接管槽位，卸载时不能把它覆盖掉。只有槽位仍
                // 精确指向本 hook 时才还原 original。
                if *self.slot == self.replacement {
                    *self.slot = self.original;
                }
                let mut back = PAGE_PROTECTION_FLAGS(0);
                let _ = VirtualProtect(
                    self.slot as *const core::ffi::c_void,
                    core::mem::size_of::<usize>(),
                    old,
                    &mut back,
                );
            }
        }
    }
}

// SAFETY: `slot` 指向进程内一块常驻的 vtable 内存；安装与卸载都在 probe 自己的
// 生命周期管理下串行发生。
#[cfg(windows)]
unsafe impl Send for VtableHook {}
#[cfg(windows)]
unsafe impl Sync for VtableHook {}

/// `IDXGISwapChain` 的 vtable 索引。
///
/// **这些数字社区通用但没有官方文档背书**（77 号 §14.2）。因此 [`verify_layout`]
/// 会在运行时核对：拿一个真实的 swapchain，检查我们打算换掉的那一项确实落在
/// 该 DLL 的模块范围内。核对不过就拒绝启动——硬编码一个错索引会 hook 到别的函数上，
/// 后果不可预期。
pub mod dxgi_swapchain {
    /// `IUnknown`(3) + `IDXGIObject`(4) + `IDXGIDeviceSubObject`(1) = 8
    pub const PRESENT: usize = 8;
    /// `Present`(8) + GetBuffer/SetFullscreenState/GetFullscreenState/GetDesc(4) = 12 → ResizeBuffers = 13
    pub const RESIZE_BUFFERS: usize = 13;
}

/// `ID3D11DeviceContext` 的 vtable 索引（同样是社区约定）。
pub mod d3d11_context {
    /// `IUnknown`(3) + `ID3D11DeviceChild`(4) = 7 → 第一个 context 方法 VSSetConstantBuffers = 7
    pub const VS_SET_CONSTANT_BUFFERS: usize = 7;
    pub const DRAW_INDEXED: usize = 12;
    pub const DRAW: usize = 13;
    pub const DRAW_INDEXED_INSTANCED: usize = 20;
    pub const DRAW_INSTANCED: usize = 21;
    /// Query commands make ExecuteCommandList failure observable only through the debug layer;
    /// discovery therefore hooks both and keeps any containing list opaque.
    pub const BEGIN: usize = 27;
    pub const END: usize = 28;
    /// `Draw`(13) 之后就是 `Map`。P4 用它截获常量缓冲写入（77 号 §5.11.1）。
    pub const MAP: usize = 14;
    pub const UNMAP: usize = 15;
    pub const OM_SET_RENDER_TARGETS: usize = 33;
    pub const OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS: usize = 34;
    pub const DRAW_AUTO: usize = 38;
    pub const DRAW_INDEXED_INSTANCED_INDIRECT: usize = 39;
    pub const DRAW_INSTANCED_INDIRECT: usize = 40;
    /// P4：default usage 缓冲走这条路（dynamic 缓冲走 Map/Unmap）。
    pub const UPDATE_SUBRESOURCE: usize = 48;
    pub const CLEAR_DEPTH_STENCIL_VIEW: usize = 53;
    /// `ClearState` resets every pipeline binding, including the active DSV. A deferred-command
    /// discovery ledger must observe it or a later Draw can be attributed to a stale view.
    pub const CLEAR_STATE: usize = 110;
    /// Deferred contexts can hide every OM/clear/draw callback inside a pre-recorded list. Until
    /// discovery has a bounded replay ledger, seeing this call must make the report fail closed.
    pub const EXECUTE_COMMAND_LIST: usize = 58;
    /// Last method on the base `ID3D11DeviceContext` interface.
    pub const FINISH_COMMAND_LIST: usize = super::D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES - 1;
    /// First Context1 methods follow the complete base-context vtable; four methods precede
    /// `VSSetConstantBuffers1`.
    pub const VS_SET_CONSTANT_BUFFERS1: usize = super::D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES + 4;
    /// Context1 can atomically replace the complete immediate-context state, including OM/DSV.
    pub const SWAP_DEVICE_CONTEXT_STATE: usize = super::D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES + 16;
    /// Context1's generic clear accepts an `ID3D11View`, including a depth-stencil view.
    pub const CLEAR_VIEW: usize = super::D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES + 17;
}

pub mod d3d11_multithread {
    /// IUnknown(3), Enter, Leave, then SetMultithreadProtected.
    pub const SET_MULTITHREAD_PROTECTED: usize = 5;
}

/// 运行时核对一项 vtable 槽位是否指向 `module_name` 所在的模块。
///
/// 这是 77 号 §5.2「vtable 索引不得硬编码，实现必须在运行时按接口 vtable 布局解析并断言」
/// 的落地：我们没法从官方文档拿到索引，但可以验证「那一项确实来自 `dxgi.dll` /
/// `d3d11.dll`」——索引若错到越出接口，指向的多半就不是这两个模块。
#[cfg(windows)]
pub fn verify_layout(
    object: *mut core::ffi::c_void,
    index: usize,
    module_name: &str,
) -> Result<(), String> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };

    let target = unsafe {
        let vtable = *(object as *mut *mut usize);
        *vtable.add(index)
    };
    if target == 0 {
        return Err(format!("vtable[{index}] 为空指针"));
    }

    let mut module = HMODULE::default();
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(target as *const u16),
            &mut module,
        )
        .map_err(|e| format!("vtable[{index}] 指向的地址不属于任何已加载模块: {e}"))?;
    }

    let mut buf = [0u16; 260];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let path = String::from_utf16_lossy(&buf[..len]).to_ascii_lowercase();
    if path.contains(&module_name.to_ascii_lowercase()) {
        Ok(())
    } else {
        Err(format!(
            "vtable[{index}] 指向 {path}，期望 {module_name}——索引很可能不对，拒绝 hook"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    fn vtable_index(offset: usize) -> usize {
        offset / std::mem::size_of::<usize>()
    }

    #[test]
    fn documented_indices_match_the_interface_layouts() {
        // IUnknown(3) + IDXGIObject(4) + IDXGIDeviceSubObject(1) = 8
        assert_eq!(dxgi_swapchain::PRESENT, 8);
        // Present + GetBuffer + SetFullscreenState + GetFullscreenState + GetDesc = 13
        assert_eq!(dxgi_swapchain::RESIZE_BUFFERS, 13);
        assert_eq!(d3d11_context::VS_SET_CONSTANT_BUFFERS, 7);
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11Multithread_Vtbl,
                SetMultithreadProtected
            )),
            d3d11_multithread::SET_MULTITHREAD_PROTECTED
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext1_Vtbl,
                VSSetConstantBuffers1
            )),
            d3d11_context::VS_SET_CONSTANT_BUFFERS1
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext1_Vtbl,
                SwapDeviceContextState
            )),
            d3d11_context::SWAP_DEVICE_CONTEXT_STATE
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(ID3D11DeviceContext1_Vtbl, ClearView)),
            d3d11_context::CLEAR_VIEW
        );
        assert_eq!(d3d11_context::DRAW_INDEXED, 12);
        assert_eq!(d3d11_context::DRAW, 13);
        assert_eq!(d3d11_context::DRAW_INDEXED_INSTANCED, 20);
        assert_eq!(d3d11_context::DRAW_INSTANCED, 21);
        assert_eq!(
            vtable_index(std::mem::offset_of!(ID3D11DeviceContext_Vtbl, Begin)),
            d3d11_context::BEGIN
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(ID3D11DeviceContext_Vtbl, End)),
            d3d11_context::END
        );
        assert_eq!(d3d11_context::OM_SET_RENDER_TARGETS, 33);
        assert_eq!(
            d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS,
            34
        );
        assert_eq!(d3d11_context::DRAW_AUTO, 38);
        assert_eq!(d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT, 39);
        assert_eq!(d3d11_context::DRAW_INSTANCED_INDIRECT, 40);
        assert_eq!(d3d11_context::EXECUTE_COMMAND_LIST, 58);
        assert_eq!(
            vtable_index(std::mem::offset_of!(ID3D11DeviceContext_Vtbl, ClearState)),
            d3d11_context::CLEAR_STATE
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                FinishCommandList
            )),
            d3d11_context::FINISH_COMMAND_LIST
        );
        const {
            assert!(D3D11_DEVICE_CONTEXT_VTABLE_ENTRIES < D3D11_DEVICE_CONTEXT1_VTABLE_ENTRIES);
            assert!(D3D11_DEVICE_CONTEXT1_VTABLE_ENTRIES < D3D11_DEVICE_CONTEXT2_VTABLE_ENTRIES);
            assert!(D3D11_DEVICE_CONTEXT2_VTABLE_ENTRIES < D3D11_DEVICE_CONTEXT3_VTABLE_ENTRIES);
            assert!(D3D11_DEVICE_CONTEXT3_VTABLE_ENTRIES < D3D11_DEVICE_CONTEXT4_VTABLE_ENTRIES);
        }

        // 不只抄一遍数字：直接拿 windows-rs 生成的官方 ABI vtable 偏移交叉核对。
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                DrawIndexedInstanced
            )),
            d3d11_context::DRAW_INDEXED_INSTANCED
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                DrawInstanced
            )),
            d3d11_context::DRAW_INSTANCED
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                OMSetRenderTargetsAndUnorderedAccessViews
            )),
            d3d11_context::OM_SET_RENDER_TARGETS_AND_UNORDERED_ACCESS_VIEWS
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(ID3D11DeviceContext_Vtbl, DrawAuto)),
            d3d11_context::DRAW_AUTO
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                DrawIndexedInstancedIndirect
            )),
            d3d11_context::DRAW_INDEXED_INSTANCED_INDIRECT
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                DrawInstancedIndirect
            )),
            d3d11_context::DRAW_INSTANCED_INDIRECT
        );
        assert_eq!(
            vtable_index(std::mem::offset_of!(
                ID3D11DeviceContext_Vtbl,
                ExecuteCommandList
            )),
            d3d11_context::EXECUTE_COMMAND_LIST
        );
        // 这两个常量只是社区约定；运行时由 verify_layout 兜底（§14.2）
        const {
            assert!(d3d11_context::OM_SET_RENDER_TARGETS > d3d11_context::DRAW);
            assert!(d3d11_context::CLEAR_DEPTH_STENCIL_VIEW > d3d11_context::OM_SET_RENDER_TARGETS);
        }
    }

    #[test]
    fn slot_state_only_allows_exact_original_to_be_rearmed() {
        assert_eq!(
            classify_slot_state(0x1000, 0x1000, 0x2000, 0x3000, 0x3000),
            VtableSlotState::Installed
        );
        assert_eq!(
            classify_slot_state(0x1000, 0x1000, 0x2000, 0x3000, 0x2000),
            VtableSlotState::RestoredOriginal
        );
        assert_eq!(
            classify_slot_state(0x1000, 0x1000, 0x2000, 0x3000, 0x4000),
            VtableSlotState::ReplacedUnknown { current: 0x4000 }
        );
        assert_eq!(
            classify_slot_state(0x1000, 0x1010, 0x2000, 0x3000, 0x5000),
            VtableSlotState::DifferentSlot {
                expected_slot: 0x1000,
                actual_slot: 0x1010,
                current: 0x5000,
            }
        );
    }

    #[test]
    fn restored_original_is_rearmed_but_unknown_owner_is_preserved() {
        unsafe {
            let allocation = VirtualAlloc(None, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
            assert!(!allocation.is_null());
            let vtable = allocation.cast::<usize>();
            let index = 3usize;
            let original = 0x1111usize;
            let replacement = 0x2222usize;
            let unknown = 0x3333usize;
            *vtable.add(index) = original;
            let mut object_vtable = vtable;
            let object = (&raw mut object_vtable).cast::<core::ffi::c_void>();
            let published_original = AtomicUsize::new(0);

            {
                let hook = VtableHook::install_with_published_original(
                    object,
                    index,
                    replacement,
                    &published_original,
                )
                .unwrap();
                assert_eq!(published_original.load(Ordering::SeqCst), original);
                assert_eq!(hook.state(object, index), VtableSlotState::Installed);

                *vtable.add(index) = original;
                assert_eq!(hook.state(object, index), VtableSlotState::RestoredOriginal);
                assert_eq!(hook.rearm_if_restored(object, index), Ok(true));
                assert_eq!(*vtable.add(index), replacement);

                *vtable.add(index) = unknown;
                assert!(hook.rearm_if_restored(object, index).is_err());
            }

            // Drop 不能覆盖在我们之后接管槽位的组件。
            assert_eq!(*vtable.add(index), unknown);
            VirtualFree(allocation, 0, MEM_RELEASE).unwrap();
        }
    }

    #[test]
    fn shadow_vtable_isolated_from_backing_table_rewrites_and_rearms_one_owner_pointer() {
        let original_draw = 0x1111usize;
        let replacement_draw = 0x2222usize;
        let unknown = 0x3333usize;
        let mut backing = vec![0usize; 8];
        backing[3] = replacement_draw;
        let backing_ptr = backing.as_mut_ptr();
        let mut object_vtable = backing_ptr;
        let object = (&raw mut object_vtable).cast::<core::ffi::c_void>();

        let shadow = unsafe { VtableShadow::capture_and_activate(object, backing.len()) }.unwrap();
        assert_eq!(shadow.owner_state(), VtableOwnerState::Active);
        assert_ne!(object_vtable, backing_ptr);
        assert_eq!(
            shadow.slot_state(3, original_draw, replacement_draw),
            VtableSlotState::Installed
        );

        // 模拟 D3D11 runtime 在 Present 中重铺内嵌 backing table；shadow 不受影响。
        backing[3] = original_draw;
        assert_eq!(shadow.backing_target(3), original_draw);
        assert_eq!(
            shadow.slot_state(3, original_draw, replacement_draw),
            VtableSlotState::Installed
        );

        // 若只恢复对象首指针，一次 CAS 即可恢复整张 shadow，不做七次页保护切换。
        object_vtable = backing_ptr;
        assert_eq!(shadow.owner_state(), VtableOwnerState::RestoredOriginal);
        assert_eq!(unsafe { shadow.rearm_if_restored() }, Ok(true));
        assert_eq!(shadow.owner_state(), VtableOwnerState::Active);

        // shadow 自身若出现未知目标仍拒绝接链。
        let shadow_slot = unsafe { object_vtable.add(3) };
        unsafe { AtomicUsize::from_ptr(shadow_slot) }.store(unknown, Ordering::Release);
        assert_eq!(
            shadow.slot_state(3, original_draw, replacement_draw),
            VtableSlotState::ReplacedUnknown { current: unknown }
        );
        assert!(
            shadow
                .rearm_slot_if_restored(3, original_draw, replacement_draw)
                .is_err()
        );
    }
}

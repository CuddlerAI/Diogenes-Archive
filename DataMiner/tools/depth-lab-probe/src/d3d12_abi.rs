//! ABI-derived D3D12 hook surface for issue #735.
//!
//! These indices are computed from the `windows` crate's generated COM vtable
//! layouts. Keeping the derivation here avoids copying community-maintained
//! numeric indices into the future injected backend.

#[cfg(test)]
use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandList_Vtbl, ID3D12Object_Vtbl, ID3D12Pageable_Vtbl,
};
use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandQueue_Vtbl, ID3D12Device_Vtbl, ID3D12Device4_Vtbl, ID3D12Device8_Vtbl,
    ID3D12Device10_Vtbl, ID3D12GraphicsCommandList_Vtbl, ID3D12Resource_Vtbl,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ResourceId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DescriptorHandle(pub u64);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct QueueId(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceState {
    Common,
    DepthWrite,
    CopySource,
    Present,
    RenderTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DsvIdentity {
    pub handle: DescriptorHandle,
    pub descriptor_generation: u64,
    pub resource: ResourceId,
    pub resource_epoch: u64,
}

const fn index(offset: usize) -> usize {
    offset / core::mem::size_of::<usize>()
}

pub const DEVICE_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D12Device_Vtbl>() / core::mem::size_of::<usize>();
pub const COMMAND_QUEUE_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D12CommandQueue_Vtbl>() / core::mem::size_of::<usize>();
pub const GRAPHICS_COMMAND_LIST_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D12GraphicsCommandList_Vtbl>() / core::mem::size_of::<usize>();
pub const RESOURCE_VTABLE_ENTRIES: usize =
    core::mem::size_of::<ID3D12Resource_Vtbl>() / core::mem::size_of::<usize>();

pub mod device {
    use super::*;

    pub const CREATE_COMMAND_QUEUE: usize =
        index(core::mem::offset_of!(ID3D12Device_Vtbl, CreateCommandQueue));
    pub const CREATE_COMMAND_LIST: usize =
        index(core::mem::offset_of!(ID3D12Device_Vtbl, CreateCommandList));
    pub const CREATE_DESCRIPTOR_HEAP: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateDescriptorHeap
    ));
    pub const CREATE_ROOT_SIGNATURE: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateRootSignature
    ));
    pub const CREATE_CONSTANT_BUFFER_VIEW: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateConstantBufferView
    ));
    pub const CREATE_SHADER_RESOURCE_VIEW: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateShaderResourceView
    ));
    pub const CREATE_UNORDERED_ACCESS_VIEW: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateUnorderedAccessView
    ));
    pub const COPY_DESCRIPTORS: usize =
        index(core::mem::offset_of!(ID3D12Device_Vtbl, CopyDescriptors));
    pub const COPY_DESCRIPTORS_SIMPLE: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CopyDescriptorsSimple
    ));
    pub const CREATE_DEPTH_STENCIL_VIEW: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateDepthStencilView
    ));
    pub const CREATE_COMMITTED_RESOURCE: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreateCommittedResource
    ));
    pub const CREATE_PLACED_RESOURCE: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        CreatePlacedResource
    ));
    pub const CREATE_FENCE: usize = index(core::mem::offset_of!(ID3D12Device_Vtbl, CreateFence));
    pub const GET_DEVICE_REMOVED_REASON: usize = index(core::mem::offset_of!(
        ID3D12Device_Vtbl,
        GetDeviceRemovedReason
    ));
}

pub mod device4 {
    use super::*;

    pub const CREATE_COMMITTED_RESOURCE1: usize = index(core::mem::offset_of!(
        ID3D12Device4_Vtbl,
        CreateCommittedResource1
    ));
}

pub mod device8 {
    use super::*;

    pub const CREATE_COMMITTED_RESOURCE2: usize = index(core::mem::offset_of!(
        ID3D12Device8_Vtbl,
        CreateCommittedResource2
    ));
    pub const CREATE_PLACED_RESOURCE1: usize = index(core::mem::offset_of!(
        ID3D12Device8_Vtbl,
        CreatePlacedResource1
    ));
}

pub mod device10 {
    use super::*;

    pub const CREATE_COMMITTED_RESOURCE3: usize = index(core::mem::offset_of!(
        ID3D12Device10_Vtbl,
        CreateCommittedResource3
    ));
    pub const CREATE_PLACED_RESOURCE2: usize = index(core::mem::offset_of!(
        ID3D12Device10_Vtbl,
        CreatePlacedResource2
    ));
}

pub mod command_queue {
    use super::*;

    pub const EXECUTE_COMMAND_LISTS: usize = index(core::mem::offset_of!(
        ID3D12CommandQueue_Vtbl,
        ExecuteCommandLists
    ));
    pub const SIGNAL: usize = index(core::mem::offset_of!(ID3D12CommandQueue_Vtbl, Signal));
    pub const WAIT: usize = index(core::mem::offset_of!(ID3D12CommandQueue_Vtbl, Wait));
}

pub mod graphics_command_list {
    use super::*;

    pub const CLOSE: usize = index(core::mem::offset_of!(ID3D12GraphicsCommandList_Vtbl, Close));
    pub const RESET: usize = index(core::mem::offset_of!(ID3D12GraphicsCommandList_Vtbl, Reset));
    pub const RS_SET_SCISSOR_RECTS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        RSSetScissorRects
    ));
    pub const RS_SET_VIEWPORTS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        RSSetViewports
    ));
    pub const DRAW_INSTANCED: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        DrawInstanced
    ));
    pub const DRAW_INDEXED_INSTANCED: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        DrawIndexedInstanced
    ));
    pub const COPY_BUFFER_REGION: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        CopyBufferRegion
    ));
    pub const COPY_TEXTURE_REGION: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        CopyTextureRegion
    ));
    pub const SET_VIEWPORTS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        RSSetViewports
    ));
    pub const SET_SCISSOR_RECTS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        RSSetScissorRects
    ));
    pub const RESOURCE_BARRIER: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        ResourceBarrier
    ));
    pub const EXECUTE_BUNDLE: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        ExecuteBundle
    ));
    pub const SET_DESCRIPTOR_HEAPS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        SetDescriptorHeaps
    ));
    pub const SET_GRAPHICS_ROOT_SIGNATURE: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        SetGraphicsRootSignature
    ));
    pub const SET_GRAPHICS_ROOT_DESCRIPTOR_TABLE: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        SetGraphicsRootDescriptorTable
    ));
    pub const SET_GRAPHICS_ROOT_32BIT_CONSTANT: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        SetGraphicsRoot32BitConstant
    ));
    pub const SET_GRAPHICS_ROOT_32BIT_CONSTANTS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        SetGraphicsRoot32BitConstants
    ));
    pub const SET_GRAPHICS_ROOT_CONSTANT_BUFFER_VIEW: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        SetGraphicsRootConstantBufferView
    ));
    pub const OM_SET_RENDER_TARGETS: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        OMSetRenderTargets
    ));
    pub const CLEAR_DEPTH_STENCIL_VIEW: usize = index(core::mem::offset_of!(
        ID3D12GraphicsCommandList_Vtbl,
        ClearDepthStencilView
    ));
}

pub mod resource {
    use super::*;

    pub const MAP: usize = index(core::mem::offset_of!(ID3D12Resource_Vtbl, Map));
    pub const UNMAP: usize = index(core::mem::offset_of!(ID3D12Resource_Vtbl, Unmap));
    pub const GET_DESC: usize = index(core::mem::offset_of!(ID3D12Resource_Vtbl, GetDesc));
    pub const GET_GPU_VIRTUAL_ADDRESS: usize = index(core::mem::offset_of!(
        ID3D12Resource_Vtbl,
        GetGPUVirtualAddress
    ));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HookSurfaceEntry {
    pub interface: &'static str,
    pub method: &'static str,
    pub index: usize,
}

/// Minimum observation surface for the first feasibility backend. It records
/// facts only; snapshot/watermark injection is enabled behind later gates.
pub const FEASIBILITY_OBSERVATION_SURFACE: [HookSurfaceEntry; 14] = [
    HookSurfaceEntry {
        interface: "ID3D12Device",
        method: "CreateCommandQueue",
        index: device::CREATE_COMMAND_QUEUE,
    },
    HookSurfaceEntry {
        interface: "ID3D12Device",
        method: "CreateCommandList",
        index: device::CREATE_COMMAND_LIST,
    },
    HookSurfaceEntry {
        interface: "ID3D12Device",
        method: "CreateDepthStencilView",
        index: device::CREATE_DEPTH_STENCIL_VIEW,
    },
    HookSurfaceEntry {
        interface: "ID3D12CommandQueue",
        method: "ExecuteCommandLists",
        index: command_queue::EXECUTE_COMMAND_LISTS,
    },
    HookSurfaceEntry {
        interface: "ID3D12CommandQueue",
        method: "Signal",
        index: command_queue::SIGNAL,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "DrawInstanced",
        index: graphics_command_list::DRAW_INSTANCED,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "DrawIndexedInstanced",
        index: graphics_command_list::DRAW_INDEXED_INSTANCED,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "RSSetViewports",
        index: graphics_command_list::SET_VIEWPORTS,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "ResourceBarrier",
        index: graphics_command_list::RESOURCE_BARRIER,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "ExecuteBundle",
        index: graphics_command_list::EXECUTE_BUNDLE,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "SetGraphicsRootSignature",
        index: graphics_command_list::SET_GRAPHICS_ROOT_SIGNATURE,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "SetGraphicsRootConstantBufferView",
        index: graphics_command_list::SET_GRAPHICS_ROOT_CONSTANT_BUFFER_VIEW,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "OMSetRenderTargets",
        index: graphics_command_list::OM_SET_RENDER_TARGETS,
    },
    HookSurfaceEntry {
        interface: "ID3D12GraphicsCommandList",
        method: "ClearDepthStencilView",
        index: graphics_command_list::CLEAR_DEPTH_STENCIL_VIEW,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_derived_methods_follow_the_complete_base_interfaces() {
        assert_eq!(
            index(core::mem::offset_of!(ID3D12Device_Vtbl, GetNodeCount)),
            core::mem::size_of::<ID3D12Object_Vtbl>() / core::mem::size_of::<usize>()
        );
        assert_eq!(
            index(core::mem::offset_of!(
                ID3D12CommandQueue_Vtbl,
                UpdateTileMappings
            )),
            core::mem::size_of::<ID3D12Pageable_Vtbl>() / core::mem::size_of::<usize>()
        );
        assert_eq!(
            graphics_command_list::CLOSE,
            core::mem::size_of::<ID3D12CommandList_Vtbl>() / core::mem::size_of::<usize>()
        );
        assert_eq!(
            resource::MAP,
            core::mem::size_of::<ID3D12Pageable_Vtbl>() / core::mem::size_of::<usize>()
        );
    }

    #[test]
    fn all_feasibility_slots_are_in_bounds_and_unique_per_interface() {
        for entry in FEASIBILITY_OBSERVATION_SURFACE {
            let entry_count = match entry.interface {
                "ID3D12Device" => DEVICE_VTABLE_ENTRIES,
                "ID3D12CommandQueue" => COMMAND_QUEUE_VTABLE_ENTRIES,
                "ID3D12GraphicsCommandList" => GRAPHICS_COMMAND_LIST_VTABLE_ENTRIES,
                other => panic!("unexpected interface {other}"),
            };
            assert!(
                entry.index < entry_count,
                "{}::{} is outside the generated vtable",
                entry.interface,
                entry.method
            );
        }

        for (index, left) in FEASIBILITY_OBSERVATION_SURFACE.iter().enumerate() {
            for right in &FEASIBILITY_OBSERVATION_SURFACE[index + 1..] {
                if left.interface == right.interface {
                    assert_ne!(
                        left.index, right.index,
                        "{} methods {} and {} share one slot",
                        left.interface, left.method, right.method
                    );
                }
            }
        }
    }

    #[test]
    fn observation_order_matches_the_generated_interface_contract() {
        const {
            assert!(device::CREATE_COMMAND_QUEUE < device::CREATE_COMMAND_LIST);
            assert!(device::CREATE_COMMAND_LIST < device::CREATE_DEPTH_STENCIL_VIEW);
            assert!(
                graphics_command_list::DRAW_INSTANCED
                    < graphics_command_list::DRAW_INDEXED_INSTANCED
            );
            assert!(
                graphics_command_list::COPY_TEXTURE_REGION
                    < graphics_command_list::RESOURCE_BARRIER
            );
            assert!(
                graphics_command_list::RESOURCE_BARRIER
                    < graphics_command_list::OM_SET_RENDER_TARGETS
            );
            assert!(
                graphics_command_list::OM_SET_RENDER_TARGETS
                    < graphics_command_list::CLEAR_DEPTH_STENCIL_VIEW
            );
            assert!(command_queue::EXECUTE_COMMAND_LISTS < command_queue::SIGNAL);
            assert!(command_queue::SIGNAL < command_queue::WAIT);
        }
    }

    #[test]
    fn render_camera_descriptor_slots_are_derived_and_in_bounds() {
        let device_slots = [
            device::CREATE_DESCRIPTOR_HEAP,
            device::CREATE_CONSTANT_BUFFER_VIEW,
            device::CREATE_SHADER_RESOURCE_VIEW,
            device::CREATE_UNORDERED_ACCESS_VIEW,
            device::COPY_DESCRIPTORS,
            device::COPY_DESCRIPTORS_SIMPLE,
        ];
        assert!(
            device_slots
                .iter()
                .all(|slot| *slot < DEVICE_VTABLE_ENTRIES)
        );
        let list_slots = [
            graphics_command_list::SET_DESCRIPTOR_HEAPS,
            graphics_command_list::SET_GRAPHICS_ROOT_SIGNATURE,
            graphics_command_list::SET_GRAPHICS_ROOT_DESCRIPTOR_TABLE,
            graphics_command_list::SET_GRAPHICS_ROOT_CONSTANT_BUFFER_VIEW,
        ];
        assert!(
            list_slots
                .iter()
                .all(|slot| *slot < GRAPHICS_COMMAND_LIST_VTABLE_ENTRIES)
        );
    }
}

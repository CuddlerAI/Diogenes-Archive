//! Non-blocking D3D12 Present watermark command ring.
//!
//! Every write uses a private allocator/list slot whose prior fence has already
//! completed. The selected backbuffer is transitioned PRESENT -> RENDER_TARGET,
//! cleared only inside the shared watermark rectangles, and restored to PRESENT
//! before submission on the exact DIRECT queue. No wait occurs in `write`.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_R10G10B10A2_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
};
use windows::core::Interface;

use depth_lab::watermark;

pub const MIN_WATERMARK_SLOTS: usize = 3;
pub const MAX_WATERMARK_SLOTS: usize = 8;

const WHITE: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatermarkTicket {
    pub slot: usize,
    pub fence: u64,
    pub present_seq: u16,
    pub target_index: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WatermarkWriteResult {
    Submitted(WatermarkTicket),
    DroppedRingBusy,
}

struct Target {
    resource: ID3D12Resource,
    rtv: D3D12_CPU_DESCRIPTOR_HANDLE,
    width: u64,
    height: u32,
}

struct Slot {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: u64,
    // The watermark geometry is fixed (24 blocks), so keep the translated
    // D3D12 RECTs with the command slot. This avoids two heap allocations on
    // every Present while preserving the shared encode contract exactly.
    white: Vec<RECT>,
    black: Vec<RECT>,
}

pub struct D3d12WatermarkRing {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    _rtv_heap: ID3D12DescriptorHeap,
    targets: Vec<Target>,
    slots: Vec<Slot>,
    fence: ID3D12Fence,
    next_fence: u64,
    dropped_busy: u64,
    fatal: Option<String>,
}

impl Drop for D3d12WatermarkRing {
    fn drop(&mut self) {
        if self.can_invalidate() {
            return;
        }
        // `write` never waits. If a caller drops before completion (or Signal
        // failed after Execute), retain one extra COM reference rather than
        // releasing objects that may still be referenced by GPU work.
        std::mem::forget(self.device.clone());
        std::mem::forget(self.queue.clone());
        std::mem::forget(self._rtv_heap.clone());
        std::mem::forget(self.fence.clone());
        for target in &self.targets {
            std::mem::forget(target.resource.clone());
        }
        for slot in &self.slots {
            std::mem::forget(slot.allocator.clone());
            std::mem::forget(slot.list.clone());
        }
    }
}

impl D3d12WatermarkRing {
    pub fn new(
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        resources: &[ID3D12Resource],
        slot_count: usize,
    ) -> Result<Self, String> {
        if resources.is_empty() || resources.len() > 16 {
            return Err("D3D12 watermark requires 1..=16 backbuffers".into());
        }
        let queue_desc = unsafe { queue.GetDesc() };
        if queue_desc.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return Err("D3D12 watermark queue is not DIRECT".into());
        }
        let mut queue_device = None;
        unsafe { queue.GetDevice(&mut queue_device) }
            .map_err(|error| format!("watermark queue GetDevice failed: {error}"))?;
        let queue_device: ID3D12Device =
            queue_device.ok_or("watermark queue GetDevice returned no device")?;
        if queue_device.as_raw() != device.as_raw() {
            return Err("watermark queue belongs to a different device".into());
        }

        let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
            Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
            NumDescriptors: resources.len() as u32,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
            NodeMask: 0,
        };
        let heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&heap_desc) }
            .map_err(|error| format!("creating watermark RTV heap failed: {error}"))?;
        let start = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
        let increment =
            unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) }
                as usize;
        if start.ptr == 0 || increment == 0 {
            return Err("watermark RTV heap returned an invalid handle/increment".into());
        }
        let mut targets = Vec::with_capacity(resources.len());
        for (index, resource) in resources.iter().enumerate() {
            let mut resource_device = None;
            unsafe { resource.GetDevice(&mut resource_device) }
                .map_err(|error| format!("watermark target GetDevice failed: {error}"))?;
            let resource_device: ID3D12Device =
                resource_device.ok_or("watermark target GetDevice returned no device")?;
            if resource_device.as_raw() != device.as_raw() {
                return Err("watermark target belongs to a different device".into());
            }
            let desc = unsafe { resource.GetDesc() };
            validate_target_desc(&desc)?;
            let rtv = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start
                    .ptr
                    .checked_add(index.saturating_mul(increment))
                    .ok_or("watermark RTV handle overflow")?,
            };
            unsafe { device.CreateRenderTargetView(resource, None, rtv) };
            targets.push(Target {
                resource: resource.clone(),
                rtv,
                width: desc.Width,
                height: desc.Height,
            });
        }

        let slot_count = slot_count.clamp(MIN_WATERMARK_SLOTS, MAX_WATERMARK_SLOTS);
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            let allocator =
                unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
                    .map_err(|error| format!("creating watermark allocator failed: {error}"))?;
            let list: ID3D12GraphicsCommandList = unsafe {
                device.CreateCommandList(
                    0,
                    D3D12_COMMAND_LIST_TYPE_DIRECT,
                    &allocator,
                    None::<&ID3D12PipelineState>,
                )
            }
            .map_err(|error| format!("creating watermark command list failed: {error}"))?;
            unsafe { list.Close() }
                .map_err(|error| format!("closing initial watermark list failed: {error}"))?;
            slots.push(Slot {
                allocator,
                list,
                fence: 0,
                white: Vec::with_capacity(watermark::TOTAL_BLOCKS as usize),
                black: Vec::with_capacity(watermark::TOTAL_BLOCKS as usize),
            });
        }
        let fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| format!("creating watermark fence failed: {error}"))?;
        Ok(Self {
            device: device.clone(),
            queue: queue.clone(),
            _rtv_heap: heap,
            targets,
            slots,
            fence,
            next_fence: 1,
            dropped_busy: 0,
            fatal: None,
        })
    }

    pub fn write(
        &mut self,
        target_index: usize,
        present_seq: u16,
        origin_x: u32,
        origin_y: u32,
    ) -> Result<WatermarkWriteResult, String> {
        self.ensure_healthy()?;
        let target = match self.targets.get(target_index) {
            Some(target) => target,
            None => return Err(self.latch("watermark target index is out of range")),
        };
        let Some(right) = origin_x.checked_add(watermark::RECT_W) else {
            return Err(self.latch("watermark horizontal origin overflows u32"));
        };
        let Some(bottom) = origin_y.checked_add(watermark::RECT_H) else {
            return Err(self.latch("watermark vertical origin overflows u32"));
        };
        if u64::from(right) > target.width || bottom > target.height {
            return Err(self.latch(format!(
                "watermark rectangle exceeds target {}x{}",
                target.width, target.height
            )));
        }
        let completed = unsafe { self.fence.GetCompletedValue() };
        if completed == u64::MAX {
            let reason = unsafe { self.device.GetDeviceRemovedReason() };
            return Err(self.latch(format!("device removed during watermark write: {reason:?}")));
        }
        let Some(slot_index) = self
            .slots
            .iter()
            .position(|slot| slot.fence == 0 || slot.fence <= completed)
        else {
            self.dropped_busy = self.dropped_busy.saturating_add(1);
            return Ok(WatermarkWriteResult::DroppedRingBusy);
        };
        let fence_value = self.next_fence;
        let Some(next_fence) = self.next_fence.checked_add(1) else {
            return Err(self.latch("watermark fence overflow"));
        };
        let base: ID3D12CommandList = (|| -> Result<_, String> {
            let slot = &mut self.slots[slot_index];
            encode_into(
                present_seq,
                origin_x,
                origin_y,
                &mut slot.white,
                &mut slot.black,
            );
            unsafe { slot.allocator.Reset() }
                .map_err(|error| format!("watermark allocator Reset failed: {error}"))?;
            unsafe {
                slot.list
                    .Reset(&slot.allocator, None::<&ID3D12PipelineState>)
            }
            .map_err(|error| format!("watermark list Reset failed: {error}"))?;
            record_transition(
                &slot.list,
                &target.resource,
                D3D12_RESOURCE_STATE_PRESENT,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            );
            unsafe {
                if !slot.black.is_empty() {
                    slot.list
                        .ClearRenderTargetView(target.rtv, &BLACK, Some(&slot.black));
                }
                if !slot.white.is_empty() {
                    slot.list
                        .ClearRenderTargetView(target.rtv, &WHITE, Some(&slot.white));
                }
            }
            record_transition(
                &slot.list,
                &target.resource,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_PRESENT,
            );
            unsafe { slot.list.Close() }
                .map_err(|error| format!("watermark list Close failed: {error}"))?;
            slot.list
                .cast()
                .map_err(|error| format!("watermark list cast failed: {error}"))
        })()
        .map_err(|error| self.latch(error))?;
        unsafe { self.queue.ExecuteCommandLists(&[Some(base)]) };
        // Execute has happened: quarantine the slot until a successful Signal
        // gives it a real completion value.
        self.slots[slot_index].fence = u64::MAX;
        if let Err(error) = unsafe { self.queue.Signal(&self.fence, fence_value) } {
            return Err(self.latch(format!("watermark queue Signal failed: {error}")));
        }
        self.next_fence = next_fence;
        self.slots[slot_index].fence = fence_value;
        Ok(WatermarkWriteResult::Submitted(WatermarkTicket {
            slot: slot_index,
            fence: fence_value,
            present_seq,
            target_index,
        }))
    }

    #[must_use]
    pub fn is_complete(&self, ticket: WatermarkTicket) -> bool {
        (unsafe { self.fence.GetCompletedValue() }) >= ticket.fence
    }

    #[must_use]
    pub fn dropped_busy(&self) -> u64 {
        self.dropped_busy
    }

    #[must_use]
    pub fn fatal_error(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    /// Backbuffer references can be released for ResizeBuffers only after every
    /// private list has completed. This check never waits.
    #[must_use]
    pub fn can_invalidate(&self) -> bool {
        let completed = unsafe { self.fence.GetCompletedValue() };
        completed != u64::MAX
            && self
                .slots
                .iter()
                .all(|slot| slot.fence == 0 || slot.fence <= completed)
    }

    fn ensure_healthy(&self) -> Result<(), String> {
        match &self.fatal {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn latch(&mut self, error: impl Into<String>) -> String {
        let error = error.into();
        if self.fatal.is_none() {
            self.fatal = Some(error.clone());
        }
        error
    }
}

fn validate_target_desc(desc: &D3D12_RESOURCE_DESC) -> Result<(), String> {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D
        || desc.Width == 0
        || desc.Height == 0
        || desc.DepthOrArraySize != 1
        || desc.MipLevels != 1
    {
        return Err("unsupported watermark backbuffer geometry".into());
    }
    if desc.SampleDesc.Count != 1 || desc.SampleDesc.Quality != 0 {
        return Err("MSAA watermark backbuffers are unsupported".into());
    }
    if !desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET) {
        return Err("watermark target is not render-target capable".into());
    }
    if !allowed_format(desc.Format) {
        return Err(format!(
            "unsupported watermark backbuffer format {:?}",
            desc.Format
        ));
    }
    Ok(())
}

fn allowed_format(format: DXGI_FORMAT) -> bool {
    format == DXGI_FORMAT_R8G8B8A8_UNORM
        || format == DXGI_FORMAT_B8G8R8A8_UNORM
        || format == DXGI_FORMAT_R10G10B10A2_UNORM
        || format == DXGI_FORMAT_R16G16B16A16_FLOAT
}

fn encode_into(
    present_seq: u16,
    origin_x: u32,
    origin_y: u32,
    white: &mut Vec<RECT>,
    black: &mut Vec<RECT>,
) {
    white.clear();
    black.clear();
    let integrity = watermark::integrity_code(present_seq);
    for index in 0..watermark::TOTAL_BLOCKS {
        let bit = if index < watermark::SYNC_BITS {
            (integrity >> (watermark::SYNC_BITS - 1 - index)) & 1 == 1
        } else {
            let payload_index = index - watermark::SYNC_BITS;
            (present_seq >> (watermark::PAYLOAD_BITS - 1 - payload_index)) & 1 == 1
        };
        let left = origin_x + index * watermark::BLOCK_PX;
        let left = left as i32;
        let rect = RECT {
            left,
            top: origin_y as i32,
            right: (left as u32 + watermark::BLOCK_PX) as i32,
            bottom: (origin_y + watermark::BLOCK_PX) as i32,
        };
        let target = if bit { &mut *white } else { &mut *black };
        if let Some(previous) = target.last_mut()
            && previous.right == rect.left
            && previous.top == rect.top
            && previous.bottom == rect.bottom
        {
            previous.right = rect.right;
        } else {
            target.push(rect);
        }
    }
}

fn record_transition(
    list: &ID3D12GraphicsCommandList,
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
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        let transition = &mut *barrier.Anonymous.Transition;
        ManuallyDrop::drop(&mut transition.pResource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target_desc() -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: u64::from(watermark::RECT_W),
            Height: watermark::RECT_H,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        }
    }

    #[test]
    fn target_admission_rejects_depth_and_msaa_surfaces() {
        assert_eq!(validate_target_desc(&target_desc()), Ok(()));

        let mut depth = target_desc();
        depth.Format = windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_D32_FLOAT;
        assert!(
            validate_target_desc(&depth)
                .unwrap_err()
                .contains("unsupported watermark backbuffer format")
        );

        let mut msaa = target_desc();
        msaa.SampleDesc.Count = 4;
        assert!(validate_target_desc(&msaa).unwrap_err().contains("MSAA"));
    }

    #[test]
    fn allocation_free_rect_encoder_matches_shared_contract() {
        let mut white = Vec::with_capacity(watermark::TOTAL_BLOCKS as usize);
        let mut black = Vec::with_capacity(watermark::TOTAL_BLOCKS as usize);
        for seq in [0, 1, 0x5555, 0xaaaa, 0xabcd, u16::MAX] {
            encode_into(seq, 17, 23, &mut white, &mut black);
            let expected = watermark::encode(seq, 17, 23);
            let expand = |rects: &[RECT]| {
                rects
                    .iter()
                    .flat_map(|rect| {
                        (rect.left..rect.right)
                            .step_by(watermark::BLOCK_PX as usize)
                            .map(|left| {
                                (
                                    left,
                                    rect.top,
                                    left + watermark::BLOCK_PX as i32,
                                    rect.bottom,
                                )
                            })
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                expand(&white),
                expected
                    .white
                    .iter()
                    .map(|rect| (
                        rect.left as i32,
                        rect.top as i32,
                        rect.right as i32,
                        rect.bottom as i32
                    ))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                expand(&black),
                expected
                    .black
                    .iter()
                    .map(|rect| (
                        rect.left as i32,
                        rect.top as i32,
                        rect.right as i32,
                        rect.bottom as i32
                    ))
                    .collect::<Vec<_>>()
            );
        }
    }
}

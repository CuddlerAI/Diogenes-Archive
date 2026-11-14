//! Bounded D3D12 depth snapshot ring.
//!
//! A selected depth resource is copied on the application's own DIRECT
//! graphics command list immediately before its final transition out of
//! DEPTH_WRITE. For D24S8/D32S8 only the device-described depth plane is copied;
//! D24 is normalized to float and stencil never enters the `.d32` byte stream.
//! `on_execute_complete` is called only after the application's matching
//! `ExecuteCommandLists`; it appends a queue signal to the same DIRECT queue.
//! CPU drain only maps slots whose private fence value has completed.

use std::ffi::c_void;
use std::mem::{ManuallyDrop, size_of};

use depth_lab::decode::{DepthFormat, ZConvention};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D32_FLOAT,
    DXGI_FORMAT_D32_FLOAT_S8X24_UINT, DXGI_FORMAT_R24G8_TYPELESS,
    DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS, DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_R32G8X24_TYPELESS,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::d3d12_abi::DsvIdentity;
use crate::readback::RawDepthFrame;

pub const MIN_CAPTURE_SLOTS: usize = 4;
pub const MAX_CAPTURE_SLOTS: usize = 16;

type ResourceBarrierFn = unsafe extern "system" fn(*mut c_void, u32, *const D3D12_RESOURCE_BARRIER);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordResult {
    Recorded { slot: usize, capture_id: u64 },
    NotSelected,
    DroppedPoolFull,
}

#[derive(Debug)]
pub struct D3d12RawDepthFrame {
    pub capture_id: u64,
    /// Exact Present sequence whose rendered depth was copied. The backend
    /// opens one interval after that Present and accepts at most one clear.
    pub present_seq: u64,
    pub source: DsvIdentity,
    pub src_width: usize,
    pub src_height: usize,
    /// Device-reported format for copied subresource/plane zero.
    pub source_plane_format: i32,
    /// Device-reported bytes occupied by one source plane row, excluding pitch padding.
    pub source_plane_row_bytes: usize,
    /// Device-reported aligned pitch of the source plane readback.
    pub source_plane_row_pitch: usize,
    /// Bytes per source plane element. D32S8 plane zero is R32X32 (8 bytes),
    /// while the emitted `.d32` remains one 4-byte float per pixel.
    pub source_plane_element_bytes: usize,
    pub row_pitch: usize,
    pub bytes: Vec<u8>,
}

impl D3d12RawDepthFrame {
    /// Converts into the existing writer-compatible frame after the caller has
    /// paired this capture token with one exact Present sequence.
    #[must_use]
    pub fn into_raw_depth_frame(
        self,
        dst_width: usize,
        dst_height: usize,
        conv: ZConvention,
    ) -> RawDepthFrame {
        RawDepthFrame {
            present_seq: self.present_seq,
            src_width: self.src_width,
            src_height: self.src_height,
            row_pitch: self.row_pitch,
            format: DepthFormat::D32Float,
            dst_width,
            dst_height,
            conv,
            bytes: self.bytes,
            pooled: false,
            verify_pooled: None,
            verify_row_pitch: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotState {
    Free,
    Recorded {
        capture_id: u64,
        present_seq: u64,
        list: usize,
        // HotListSlot's draw count when this copy was recorded, not at Execute.
        draws_before_in_list: Option<u32>,
    },
    Submitted {
        capture_id: u64,
        present_seq: u64,
        list: usize,
        queue: usize,
        fence: u64,
    },
}

struct Slot {
    readback: ID3D12Resource,
    state: SlotState,
    gpu_lease: Option<Option<(ID3D12Fence, u64)>>,
}

/// A GPU-only snapshot, leased until its private compute consumer completes.
pub struct GpuDepthSnapshot {
    pub slot: usize,
    pub capture_id: u64,
    pub present_seq: u64,
    pub queue: usize,
    pub resource: ID3D12Resource,
    pub width: u32,
    pub height: u32,
    pub row_pitch: u32,
    pub element_bytes: u32,
    pub encoding: DepthPlaneEncoding,
}

/// Encoding of plane-zero bytes in the placed copy footprint, independent of its R32 format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum DepthPlaneEncoding {
    Float32 = 0,
    Unorm24 = 1,
}

const DEPTH_PLANE_SUBRESOURCE: u32 = 0;

#[derive(Clone, Copy)]
struct DepthPlaneLayout {
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
    rows: usize,
    source_row_bytes: usize,
    source_element_bytes: usize,
    output_row_bytes: usize,
    total_bytes: u64,
    encoding: DepthPlaneEncoding,
}

impl DepthPlaneLayout {
    fn query(
        device: &ID3D12Device,
        desc: &D3D12_RESOURCE_DESC,
        expected_plane_count: u8,
    ) -> Result<Self, String> {
        let mut format_info = D3D12_FEATURE_DATA_FORMAT_INFO {
            Format: desc.Format,
            PlaneCount: 0,
        };
        unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_FORMAT_INFO,
                std::ptr::from_mut(&mut format_info).cast(),
                u32::try_from(size_of::<D3D12_FEATURE_DATA_FORMAT_INFO>())
                    .map_err(|_| "D3D12 format-info size exceeds u32".to_string())?,
            )
        }
        .map_err(|error| format!("querying D3D12 depth plane count failed: {error}"))?;
        if format_info.PlaneCount != expected_plane_count {
            return Err(format!(
                "unexpected plane count {} for {:?}; expected {expected_plane_count}",
                format_info.PlaneCount, desc.Format
            ));
        }

        // With one mip and one array slice, D3D12 subresource zero is exactly
        // plane slice zero. GetCopyableFootprints is the source of truth for
        // the plane's row size, row pitch, and allocation span.
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut rows = 0_u32;
        let mut row_size = 0_u64;
        let mut total_bytes = 0_u64;
        unsafe {
            device.GetCopyableFootprints(
                desc,
                DEPTH_PLANE_SUBRESOURCE,
                1,
                0,
                Some(&mut footprint),
                Some(&mut rows),
                Some(&mut row_size),
                Some(&mut total_bytes),
            );
        }
        let output_row_size = desc
            .Width
            .checked_mul(4)
            .ok_or_else(|| "D32 depth row size overflow".to_string())?;
        let encoding = if matches!(
            desc.Format,
            DXGI_FORMAT_R24G8_TYPELESS | DXGI_FORMAT_D24_UNORM_S8_UINT
        ) {
            DepthPlaneEncoding::Unorm24
        } else {
            DepthPlaneEncoding::Float32
        };
        let source_element_bytes = if encoding == DepthPlaneEncoding::Unorm24 {
            // D3D12 planar staging is D24X8 in an R32_TYPELESS footprint, not float bits.
            if footprint.Footprint.Format != DXGI_FORMAT_R32_TYPELESS {
                return Err(format!(
                    "unexpected D24 plane-0 footprint format {:?}",
                    footprint.Footprint.Format
                ));
            }
            4_u64
        } else if desc.Format == DXGI_FORMAT_R32G8X24_TYPELESS {
            if !matches!(
                footprint.Footprint.Format,
                DXGI_FORMAT_R32_TYPELESS | DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS
            ) {
                return Err(format!(
                    "unexpected D32S8 plane-0 footprint format {:?}",
                    footprint.Footprint.Format
                ));
            }
            if footprint.Footprint.Format == DXGI_FORMAT_R32_TYPELESS {
                4_u64
            } else {
                8_u64
            }
        } else {
            if footprint.Footprint.Format != desc.Format {
                return Err(format!(
                    "unexpected D32 plane-0 footprint format {:?} for {:?}",
                    footprint.Footprint.Format, desc.Format
                ));
            }
            4_u64
        };
        let expected_source_row_size = desc
            .Width
            .checked_mul(source_element_bytes)
            .ok_or_else(|| "D32 source plane row size overflow".to_string())?;
        if rows != desc.Height
            || row_size != expected_source_row_size
            || footprint.Footprint.Width != desc.Width as u32
            || footprint.Footprint.Height != desc.Height
            || footprint.Footprint.Depth != 1
            || u64::from(footprint.Footprint.RowPitch) < row_size
            || total_bytes == 0
        {
            return Err(format!(
                "unsupported D32 plane-0 footprint rows={rows} row_size={row_size} row_pitch={} width={} height={} depth={} total={total_bytes}",
                footprint.Footprint.RowPitch,
                footprint.Footprint.Width,
                footprint.Footprint.Height,
                footprint.Footprint.Depth,
            ));
        }
        Ok(Self {
            footprint,
            rows: usize::try_from(rows)
                .map_err(|_| "D32 depth row count exceeds usize".to_string())?,
            source_row_bytes: usize::try_from(row_size)
                .map_err(|_| "D32 depth row size exceeds usize".to_string())?,
            source_element_bytes: usize::try_from(source_element_bytes)
                .map_err(|_| "D32 source element size exceeds usize".to_string())?,
            output_row_bytes: usize::try_from(output_row_size)
                .map_err(|_| "tight D32 row size exceeds usize".to_string())?,
            total_bytes,
            encoding,
        })
    }
}

/// The ring owns all GPU/CPU resources up front. Recording and submission do
/// not allocate, wait, or create D3D objects.
pub struct D3d12CaptureRing {
    device: ID3D12Device,
    source: ID3D12Resource,
    source_identity: DsvIdentity,
    width: usize,
    height: usize,
    depth_plane: DepthPlaneLayout,
    slots: Vec<Slot>,
    fence: ID3D12Fence,
    next_fence: u64,
    next_capture_id: u64,
    queue_identity: Option<usize>,
    dropped_pool_full: u64,
    current_present_seq: Option<u64>,
    capture_requested: bool,
    captured_this_interval: bool,
    fatal: Option<String>,
    gpu_only: bool,
}

impl Drop for D3d12CaptureRing {
    fn drop(&mut self) {
        if self.is_idle() {
            return;
        }
        // Dropping GPU objects before completion is not made safe by a failed
        // fence signal. Leak one extra COM reference in this exceptional path;
        // process teardown will reclaim it, while the GPU can never observe a
        // freed allocator/resource through this safe Rust API.
        std::mem::forget(self.device.clone());
        std::mem::forget(self.source.clone());
        std::mem::forget(self.fence.clone());
        for slot in &self.slots {
            std::mem::forget(slot.readback.clone());
        }
    }
}

impl D3d12CaptureRing {
    pub fn new(
        device: &ID3D12Device,
        source: &ID3D12Resource,
        source_identity: DsvIdentity,
        dsv_format: DXGI_FORMAT,
        slot_count: usize,
    ) -> Result<Self, String> {
        Self::new_with_storage(
            device,
            source,
            source_identity,
            dsv_format,
            slot_count,
            false,
        )
    }

    pub fn new_with_storage(
        device: &ID3D12Device,
        source: &ID3D12Resource,
        source_identity: DsvIdentity,
        dsv_format: DXGI_FORMAT,
        slot_count: usize,
        gpu_only: bool,
    ) -> Result<Self, String> {
        if source_identity.resource_epoch == 0
            || source_identity.descriptor_generation == 0
            || source_identity.handle.0 == 0
            || source_identity.resource.0 == 0
        {
            return Err("D3D12 capture source identity contains zero fields".into());
        }
        let mut source_device = None;
        unsafe { source.GetDevice(&mut source_device) }
            .map_err(|error| format!("depth source GetDevice failed: {error}"))?;
        let source_device: ID3D12Device =
            source_device.ok_or("depth source GetDevice returned no device")?;
        if source_device.as_raw() != device.as_raw() {
            return Err("depth source belongs to a different D3D12 device".into());
        }
        let desc = unsafe { source.GetDesc() };
        validate_source_desc(&desc, dsv_format)?;

        let expected_plane_count = if matches!(
            desc.Format,
            DXGI_FORMAT_R32G8X24_TYPELESS
                | DXGI_FORMAT_R24G8_TYPELESS
                | DXGI_FORMAT_D24_UNORM_S8_UINT
        ) {
            2
        } else {
            1
        };
        let depth_plane = DepthPlaneLayout::query(device, &desc, expected_plane_count)?;

        let slot_count = slot_count.clamp(MIN_CAPTURE_SLOTS, MAX_CAPTURE_SLOTS);
        let mut slots = Vec::with_capacity(slot_count);
        for _ in 0..slot_count {
            slots.push(Slot {
                readback: create_buffer(device, depth_plane.total_bytes, gpu_only, false)?,
                state: SlotState::Free,
                gpu_lease: None,
            });
        }
        let fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
            .map_err(|error| format!("creating D3D12 capture fence failed: {error}"))?;
        Ok(Self {
            device: device.clone(),
            source: source.clone(),
            source_identity,
            width: usize::try_from(desc.Width)
                .map_err(|_| "depth width exceeds usize".to_string())?,
            height: desc.Height as usize,
            depth_plane,
            slots,
            fence,
            next_fence: 1,
            next_capture_id: 1,
            queue_identity: None,
            dropped_pool_full: 0,
            current_present_seq: None,
            capture_requested: false,
            captured_this_interval: true,
            fatal: None,
            gpu_only,
        })
    }

    pub fn gpu_storage_bytes(&self) -> u64 {
        self.depth_plane
            .total_bytes
            .div_ceil(65536)
            .saturating_mul(65536)
            .saturating_mul(self.slots.len() as u64)
    }

    /// Polls both producer and consumer fences. Never maps GPU candidates.
    pub fn take_gpu_snapshots(&mut self) -> Result<Vec<GpuDepthSnapshot>, String> {
        self.ensure_healthy()?;
        if !self.gpu_only {
            return Err("raw readback ring cannot supply GPU snapshots".into());
        }
        let completed = unsafe { self.fence.GetCompletedValue() };
        if completed == u64::MAX {
            return Err(self.latch("D3D12 snapshot device removed"));
        }
        let mut ready = Vec::new();
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if let Some(Some((fence, value))) = &slot.gpu_lease {
                let done = unsafe { fence.GetCompletedValue() };
                if done == u64::MAX {
                    return Err("D3D12 snapshot consumer device removed".into());
                }
                if done >= *value {
                    slot.state = SlotState::Free;
                    slot.gpu_lease = None;
                }
            }
            if slot.gpu_lease.is_some() {
                continue;
            }
            if let SlotState::Submitted {
                capture_id,
                present_seq,
                queue,
                fence,
                ..
            } = slot.state
                && fence != u64::MAX
            {
                slot.gpu_lease = Some(None);
                ready.push(GpuDepthSnapshot {
                    slot: index,
                    capture_id,
                    present_seq,
                    queue,
                    resource: slot.readback.clone(),
                    width: self.width as u32,
                    height: self.height as u32,
                    row_pitch: self.depth_plane.footprint.Footprint.RowPitch,
                    element_bytes: self.depth_plane.source_element_bytes as u32,
                    encoding: self.depth_plane.encoding,
                });
            }
        }
        ready.sort_by_key(|snapshot| snapshot.capture_id);
        Ok(ready)
    }

    /// None retires an unused snapshot at its producer fence; Some defers reuse until compute finishes.
    pub fn release_gpu_snapshot(
        &mut self,
        snapshot: &GpuDepthSnapshot,
        consumer: Option<(ID3D12Fence, u64)>,
    ) -> Result<(), String> {
        let slot = self
            .slots
            .get_mut(snapshot.slot)
            .ok_or("GPU snapshot slot out of bounds")?;
        if !matches!(slot.state, SlotState::Submitted { capture_id, present_seq, .. }
            if capture_id == snapshot.capture_id && present_seq == snapshot.present_seq)
            || !matches!(slot.gpu_lease, Some(None))
        {
            return Err("stale or duplicate GPU snapshot lease".into());
        }
        if let Some(completion) = consumer {
            slot.gpu_lease = Some(Some(completion));
        } else {
            let SlotState::Submitted { fence, .. } = slot.state else {
                unreachable!()
            };
            slot.gpu_lease = Some(Some((self.fence.clone(), fence)));
        }
        Ok(())
    }

    pub(crate) fn diagnostic_interval(
        &self,
        resource: *mut c_void,
    ) -> Option<(DsvIdentity, Option<u64>, bool, bool)> {
        (self.source.as_raw() == resource).then_some((
            self.source_identity,
            self.current_present_seq,
            self.capture_requested,
            self.captured_this_interval,
        ))
    }

    #[must_use]
    pub fn source_identity(&self) -> DsvIdentity {
        self.source_identity
    }

    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.slots.iter().all(|slot| slot.state == SlotState::Free)
    }

    pub(crate) fn authoring_boundary_is_drained(&self) -> bool {
        self.is_idle()
            && !self.capture_requested
            && self.slots.iter().all(|s| s.gpu_lease.is_none())
    }

    /// Reuse the same healthy allocation at an explicit diagnostic phase boundary. In
    /// particular, next_capture_id, fence values, queue identity and loss counters never reset.
    pub(crate) fn rebind_authoring_source(
        &mut self,
        source: &ID3D12Resource,
        identity: DsvIdentity,
        dsv_format: DXGI_FORMAT,
    ) -> Result<ID3D12Resource, String> {
        self.ensure_healthy()?;
        if !cfg!(all(feature = "cp2077-diagnostics", debug_assertions))
            || self.gpu_only
            || !self.authoring_boundary_is_drained()
        {
            return Err("authoring source change requires a drained diagnostic raw ring".into());
        }
        let desc = unsafe { source.GetDesc() };
        validate_source_desc(&desc, dsv_format)?;
        if desc != unsafe { self.source.GetDesc() } {
            return Err("authoring source resource description changed".into());
        }
        let mut device = None;
        unsafe { source.GetDevice(&mut device) }.map_err(|e| e.to_string())?;
        let device: ID3D12Device = device.ok_or("authoring source has no device")?;
        if device.as_raw() != self.device.as_raw()
            || identity.resource.0 == 0
            || identity.handle.0 == 0
            || identity.resource_epoch == 0
            || identity.descriptor_generation == 0
        {
            return Err("authoring source device/identity differs".into());
        }
        // Release is hooked and can acquire the camera tracker. Transfer the retired
        // reference to the backend so it drops only after both backend locks are gone.
        let retired = std::mem::replace(&mut self.source, source.clone());
        self.source_identity = identity;
        self.current_present_seq = None;
        self.captured_this_interval = true;
        Ok(retired)
    }

    #[must_use]
    pub fn dropped_pool_full(&self) -> u64 {
        self.dropped_pool_full
    }

    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.state != SlotState::Free)
            .count()
    }

    #[must_use]
    pub fn fatal_error(&self) -> Option<&str> {
        self.fatal.as_deref()
    }

    pub fn invalidate(&mut self, reason: impl Into<String>) {
        let _ = self.latch(reason);
    }

    /// Opens the clear interval following one exact Present. At most one
    /// snapshot may be recorded before the next interval is opened.
    pub fn begin_present_interval(&mut self, present_seq: u64, requested: bool) {
        self.current_present_seq = Some(present_seq);
        self.capture_requested = requested;
        self.captured_this_interval = false;
    }

    /// Close the current request without discarding recorded commands, submitted
    /// copies or GPU leases. Execute association and fenced retirement must continue.
    pub fn stop_production(&mut self) {
        self.capture_requested = false;
    }

    /// Records DEPTH_WRITE -> COPY_SOURCE -> DEPTH_WRITE and one texture copy
    /// immediately before the application's first full-resource transition
    /// out of DEPTH_WRITE. At that point the selected pass has completed, while
    /// the transient depth allocation has not yet been made readable or reused.
    /// The barrier target must be the exact original windows-rs ABI method,
    /// bypassing the observation hook.
    ///
    /// # Safety
    ///
    /// `list_ptr` must be the live list represented by `list`, and
    /// `barrier_original` must be that vtable's original ResourceBarrier target.
    pub unsafe fn record_before_depth_write_exit(
        &mut self,
        list_ptr: *mut c_void,
        list: &ID3D12GraphicsCommandList,
        barrier_original: usize,
        resource_ptr: *mut c_void,
        draws_before_in_list: Option<u32>,
    ) -> Result<RecordResult, String> {
        self.ensure_healthy()?;
        let Some(present_seq) = self.current_present_seq else {
            return Ok(RecordResult::NotSelected);
        };
        if !self.capture_requested || self.captured_this_interval {
            return Ok(RecordResult::NotSelected);
        }
        if list_ptr.is_null() || barrier_original == 0 || resource_ptr.is_null() {
            return Err(self.latch("D3D12 capture received a null list/resource/barrier target"));
        }
        if self.source.as_raw() != resource_ptr {
            return Ok(RecordResult::NotSelected);
        }
        let base: ID3D12CommandList = list
            .cast()
            .map_err(|error| self.latch(format!("graphics list cast failed: {error}")))?;
        if unsafe { base.GetType() } != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return Err(self.latch("depth capture command list is not DIRECT"));
        }
        let mut list_device = None;
        unsafe { list.GetDevice(&mut list_device) }
            .map_err(|error| self.latch(format!("command list GetDevice failed: {error}")))?;
        let list_device: ID3D12Device =
            list_device.ok_or_else(|| self.latch("command list GetDevice returned no device"))?;
        if list_device.as_raw() != self.device.as_raw() {
            return Err(self.latch("depth command list belongs to a different device"));
        }
        let Some(slot_index) = self
            .slots
            .iter()
            .position(|slot| slot.state == SlotState::Free)
        else {
            self.dropped_pool_full = self.dropped_pool_full.saturating_add(1);
            self.captured_this_interval = true;
            return Ok(RecordResult::DroppedPoolFull);
        };
        let capture_id = self.next_capture_id;
        self.next_capture_id = self
            .next_capture_id
            .checked_add(1)
            .ok_or_else(|| self.latch("D3D12 capture id overflow"))?;

        let barrier: ResourceBarrierFn = unsafe { std::mem::transmute(barrier_original) };
        unsafe {
            call_transition(
                barrier,
                list_ptr,
                &self.source,
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            record_copy(
                list,
                &self.source,
                &self.slots[slot_index].readback,
                self.depth_plane.footprint,
            );
            call_transition(
                barrier,
                list_ptr,
                &self.source,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_DEPTH_WRITE,
            );
        }
        self.slots[slot_index].state = SlotState::Recorded {
            capture_id,
            present_seq,
            list: list_ptr as usize,
            draws_before_in_list,
        };
        self.captured_this_interval = true;
        Ok(RecordResult::Recorded {
            slot: slot_index,
            capture_id,
        })
    }

    /// Records the same private DEPTH_WRITE copy at a build-pinned pass end
    /// when the application does not expose the expected transition itself.
    /// The caller must prove the exact draw identity before invoking this path.
    ///
    /// # Safety
    ///
    /// `list_ptr`, `list`, and `barrier_original` have the same requirements as
    /// [`Self::record_before_depth_write_exit`].
    pub unsafe fn record_at_proven_pass_end(
        &mut self,
        list_ptr: *mut c_void,
        list: &ID3D12GraphicsCommandList,
        barrier_original: usize,
        draws_before_in_list: Option<u32>,
    ) -> Result<RecordResult, String> {
        let source = self.source.as_raw();
        unsafe {
            self.record_before_depth_write_exit(
                list_ptr,
                list,
                barrier_original,
                source,
                draws_before_in_list,
            )
        }
    }

    /// Appends one private fence signal after the application's Execute call.
    /// Only slots recorded by the submitted lists are attached to this fence.
    /// Returns each attached copy's Present, raw command list and copy-time
    /// draw count, so callers can place the copy inside the submitted batch
    /// and inside that list's recording rather than only by Present.
    pub fn on_execute_complete(
        &mut self,
        queue: &ID3D12CommandQueue,
        list_ptrs: &[*mut c_void],
    ) -> Result<Vec<(u64, usize, Option<u32>)>, String> {
        self.ensure_healthy()?;
        let submitted_lists: std::collections::BTreeSet<usize> =
            list_ptrs.iter().map(|value| *value as usize).collect();
        let matching: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| match slot.state {
                SlotState::Recorded { list, .. } if submitted_lists.contains(&list) => Some(index),
                _ => None,
            })
            .collect();
        if matching.is_empty() {
            return Ok(Vec::new());
        }
        let present_seqs: Vec<_> = matching
            .iter()
            .filter_map(|index| match self.slots[*index].state {
                SlotState::Recorded {
                    present_seq,
                    list,
                    draws_before_in_list,
                    ..
                } => Some((present_seq, list, draws_before_in_list)),
                _ => None,
            })
            .collect();
        let queue_key = queue.as_raw() as usize;
        let queue_desc = unsafe { queue.GetDesc() };
        if queue_desc.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
            self.quarantine_submitted(&matching, queue_key);
            return Err(self.latch("depth capture Execute queue is not DIRECT"));
        }
        let mut queue_device = None;
        if let Err(error) = unsafe { queue.GetDevice(&mut queue_device) } {
            self.quarantine_submitted(&matching, queue_key);
            return Err(self.latch(format!("queue GetDevice failed: {error}")));
        }
        let Some(queue_device): Option<ID3D12Device> = queue_device else {
            self.quarantine_submitted(&matching, queue_key);
            return Err(self.latch("queue GetDevice returned no device"));
        };
        if queue_device.as_raw() != self.device.as_raw() {
            self.quarantine_submitted(&matching, queue_key);
            return Err(self.latch("depth Execute queue belongs to a different device"));
        }
        if let Some(expected) = self.queue_identity
            && expected != queue_key
        {
            self.quarantine_submitted(&matching, queue_key);
            return Err(self.latch(
                "depth capture queue drifted from the queue established by the armed clear/Execute association",
            ));
        }
        self.queue_identity.get_or_insert(queue_key);
        let Some(next_fence) = self.next_fence.checked_add(1) else {
            self.quarantine_submitted(&matching, queue_key);
            return Err(self.latch("D3D12 capture fence overflow"));
        };
        let fence_value = self.next_fence;
        // Execute already happened. Quarantine these slots before Signal so
        // any Signal failure can never make their readbacks reusable or let
        // the ring be disarmed while GPU completion is unknown.
        self.quarantine_submitted(&matching, queue_key);
        if let Err(error) = unsafe { queue.Signal(&self.fence, fence_value) } {
            return Err(self.latch(format!("D3D12 capture queue Signal failed: {error}")));
        }
        self.next_fence = next_fence;
        for index in &matching {
            let SlotState::Submitted {
                capture_id,
                present_seq,
                list,
                queue,
                fence: u64::MAX,
            } = self.slots[*index].state
            else {
                return Err(self.latch("capture slot changed during Execute association"));
            };
            self.slots[*index].state = SlotState::Submitted {
                capture_id,
                present_seq,
                list,
                queue,
                fence: fence_value,
            };
        }
        Ok(present_seqs)
    }

    fn quarantine_submitted(&mut self, indices: &[usize], queue: usize) {
        for index in indices {
            if let SlotState::Recorded {
                capture_id,
                present_seq,
                list,
                ..
            } = self.slots[*index].state
            {
                self.slots[*index].state = SlotState::Submitted {
                    capture_id,
                    present_seq,
                    list,
                    queue,
                    fence: u64::MAX,
                };
            }
        }
    }

    /// Frees commands discarded by `ID3D12GraphicsCommandList::Reset` before
    /// they ever reached Execute. Submitted slots are never touched here.
    pub fn discard_recorded_list(&mut self, list: usize) {
        for slot in &mut self.slots {
            if matches!(slot.state, SlotState::Recorded { list: owner, .. } if owner == list) {
                slot.state = SlotState::Free;
            }
        }
    }

    /// Non-blocking GPU readiness check followed by Map/copy/Unmap for completed
    /// slots. No event wait, spin, sleep, or queue flush occurs.
    pub fn drain_ready(&mut self) -> Result<Vec<D3d12RawDepthFrame>, String> {
        self.ensure_healthy()?;
        if self.gpu_only {
            return Err("GPU-only snapshots must not be CPU-mapped".into());
        }
        let completed = unsafe { self.fence.GetCompletedValue() };
        if completed == u64::MAX {
            let reason = unsafe { self.device.GetDeviceRemovedReason() };
            return Err(self.latch(format!(
                "D3D12 device removed during depth drain: {reason:?}"
            )));
        }
        let len = match usize::try_from(self.depth_plane.total_bytes) {
            Ok(len) => len,
            Err(_) => return Err(self.latch("D3D12 readback size exceeds usize")),
        };
        let completed_slots: Vec<(usize, u64, u64, ID3D12Resource)> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let SlotState::Submitted {
                    capture_id,
                    present_seq,
                    fence,
                    ..
                } = slot.state
                else {
                    return None;
                };
                (fence <= completed)
                    .then(|| (index, capture_id, present_seq, slot.readback.clone()))
            })
            .collect();
        let mut ready = Vec::with_capacity(completed_slots.len());
        for (slot_index, capture_id, present_seq, readback) in completed_slots {
            let range = D3D12_RANGE { Begin: 0, End: len };
            let mut mapped = std::ptr::null_mut();
            if let Err(error) = unsafe { readback.Map(0, Some(&range), Some(&mut mapped)) } {
                return Err(self.latch(format!("completed D3D12 readback Map failed: {error}")));
            }
            if mapped.is_null() {
                return Err(self.latch("completed D3D12 readback Map returned null"));
            }
            let mapped_bytes = unsafe { std::slice::from_raw_parts(mapped.cast::<u8>(), len) };
            let bytes = match extract_tight_depth_plane(mapped_bytes, self.depth_plane) {
                Ok(bytes) => bytes,
                Err(error) => {
                    let no_write = D3D12_RANGE { Begin: 0, End: 0 };
                    unsafe { readback.Unmap(0, Some(&no_write)) };
                    return Err(self.latch(error));
                }
            };
            let no_write = D3D12_RANGE { Begin: 0, End: 0 };
            unsafe { readback.Unmap(0, Some(&no_write)) };
            ready.push(D3d12RawDepthFrame {
                capture_id,
                present_seq,
                source: self.source_identity,
                src_width: self.width,
                src_height: self.height,
                source_plane_format: self.depth_plane.footprint.Footprint.Format.0,
                source_plane_row_bytes: self.depth_plane.source_row_bytes,
                source_plane_row_pitch: self.depth_plane.footprint.Footprint.RowPitch as usize,
                source_plane_element_bytes: self.depth_plane.source_element_bytes,
                row_pitch: self.depth_plane.output_row_bytes,
                bytes,
            });
            self.slots[slot_index].state = SlotState::Free;
        }
        ready.sort_by_key(|frame| frame.capture_id);
        Ok(ready)
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

fn validate_source_desc(desc: &D3D12_RESOURCE_DESC, dsv_format: DXGI_FORMAT) -> Result<(), String> {
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
        return Err("only Texture2D depth resources are supported".into());
    }
    if desc.Width == 0 || desc.Height == 0 {
        return Err("depth resource has empty geometry".into());
    }
    if desc.DepthOrArraySize != 1 || desc.MipLevels != 1 {
        return Err("depth arrays/mip chains are not supported".into());
    }
    if desc.SampleDesc.Count != 1 || desc.SampleDesc.Quality != 0 {
        return Err("MSAA depth requires an explicit resolve path and is unsupported".into());
    }
    if !matches!(
        (desc.Format, dsv_format),
        (DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT)
            | (DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT)
            | (DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT)
            | (DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_D24_UNORM_S8_UINT)
            | (
                DXGI_FORMAT_R32G8X24_TYPELESS,
                DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            )
    ) {
        return Err(format!(
            "unsupported D3D12 depth resource/view pair {:?}/{dsv_format:?}; expected an exact D32, D24S8 or D32S8 pair",
            desc.Format,
        ));
    }
    if !desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL) {
        return Err("selected resource is not depth-stencil capable".into());
    }
    Ok(())
}

fn extract_tight_depth_plane(mapped: &[u8], layout: DepthPlaneLayout) -> Result<Vec<u8>, String> {
    let offset = usize::try_from(layout.footprint.Offset)
        .map_err(|_| "D32 plane offset exceeds usize".to_string())?;
    let row_pitch = usize::try_from(layout.footprint.Footprint.RowPitch)
        .map_err(|_| "D32 plane row pitch exceeds usize".to_string())?;
    if layout.source_row_bytes > row_pitch {
        return Err("D32 plane row bytes exceed the device row pitch".into());
    }
    if layout.source_element_bytes < 4
        || !layout
            .source_row_bytes
            .is_multiple_of(layout.source_element_bytes)
        || layout.output_row_bytes
            != (layout.source_row_bytes / layout.source_element_bytes).saturating_mul(4)
    {
        return Err("D32 plane element/output layout is inconsistent".into());
    }
    let required = layout
        .rows
        .checked_sub(1)
        .and_then(|last| last.checked_mul(row_pitch))
        .and_then(|last| offset.checked_add(last))
        .and_then(|last| last.checked_add(layout.source_row_bytes))
        .ok_or_else(|| "D32 plane mapped range overflow".to_string())?;
    if mapped.len() < required {
        return Err(format!(
            "D32 plane mapped range is short: {} < {required}",
            mapped.len()
        ));
    }
    let dense_len = layout
        .output_row_bytes
        .checked_mul(layout.rows)
        .ok_or_else(|| "tight D32 output size overflow".to_string())?;
    let mut dense = Vec::with_capacity(dense_len);
    for row in 0..layout.rows {
        let row_start = offset + row * row_pitch;
        for element_start in
            (row_start..row_start + layout.source_row_bytes).step_by(layout.source_element_bytes)
        {
            // The admitted plane format's first component is R32_FLOAT. Its
            // following X bits (including the 32-bit X8X24 half for D32S8)
            // are deliberately excluded from the tight D32 stream.
            let bytes: [u8; 4] = mapped[element_start..element_start + 4].try_into().unwrap();
            if layout.encoding == DepthPlaneEncoding::Unorm24 {
                let value = u32::from_le_bytes(bytes);
                if value & 0xff00_0000 != 0 {
                    return Err("D24 plane copy contains nonzero reserved X8 bits".into());
                }
                dense.extend_from_slice(&((value as f32) / 16_777_215.0).to_le_bytes());
            } else {
                dense.extend_from_slice(&bytes);
            }
        }
    }
    Ok(dense)
}

pub(crate) fn create_buffer(
    device: &ID3D12Device,
    bytes: u64,
    gpu: bool,
    uav: bool,
) -> Result<ID3D12Resource, String> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: if gpu {
            D3D12_HEAP_TYPE_DEFAULT
        } else {
            D3D12_HEAP_TYPE_READBACK
        },
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_UNKNOWN,
        MemoryPoolPreference: D3D12_MEMORY_POOL_UNKNOWN,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: 0,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_UNKNOWN,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: if uav {
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
        } else {
            D3D12_RESOURCE_FLAG_NONE
        },
    };
    let mut resource = None;
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut resource,
        )
    }
    .map_err(|error| format!("creating D3D12 readback buffer failed: {error}"))?;
    resource.ok_or_else(|| "CreateCommittedResource returned no readback buffer".into())
}

unsafe fn call_transition(
    barrier_fn: ResourceBarrierFn,
    list: *mut c_void,
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
    unsafe { barrier_fn(list, 1, &barrier) };
    unsafe {
        let transition = &mut *barrier.Anonymous.Transition;
        ManuallyDrop::drop(&mut transition.pResource);
    }
}

unsafe fn record_copy(
    list: &ID3D12GraphicsCommandList,
    source: &ID3D12Resource,
    destination: &ID3D12Resource,
    footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
) {
    let _perf = crate::perf::scope(crate::perf::Section::DsvSnapshot);
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
        list.CopyTextureRegion(&destination_location, 0, 0, 0, &source_location, None);
        ManuallyDrop::drop(&mut source_location.pResource);
        ManuallyDrop::drop(&mut destination_location.pResource);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "cp2077-diagnostics", debug_assertions))]
    #[test]
    fn authoring_rebind_reuses_real_d3d12_storage_and_transfers_retired_ownership() {
        use crate::d3d12_abi::{DescriptorHandle, ResourceId};
        let (device, _) = crate::d3d12_candidate::tests::warp_device();
        let texture = |width| {
            let mut resource = None;
            let mut desc = source_desc();
            desc.Width = width;
            desc.Height = 64;
            desc.Format = DXGI_FORMAT_R32G8X24_TYPELESS;
            unsafe {
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        CreationNodeMask: 1,
                        VisibleNodeMask: 1,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    None,
                    &mut resource,
                )
            }
            .unwrap();
            resource.unwrap()
        };
        let old: ID3D12Resource = texture(64);
        let new: ID3D12Resource = texture(64);
        let identity = |n| DsvIdentity {
            handle: DescriptorHandle(n),
            resource: ResourceId(n),
            descriptor_generation: 1,
            resource_epoch: 1,
        };
        let mut ring = D3d12CaptureRing::new(
            &device,
            &old,
            identity(6),
            DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
            4,
        )
        .unwrap();
        let storage: Vec<_> = ring.slots.iter().map(|s| s.readback.as_raw()).collect();
        let fence = ring.fence.as_raw();
        ring.next_capture_id = 77;
        ring.next_fence = 101;
        ring.dropped_pool_full = 3;
        ring.queue_identity = Some(123);
        ring.begin_present_interval(50, true);
        assert!(
            ring.rebind_authoring_source(&new, identity(29), DXGI_FORMAT_D32_FLOAT_S8X24_UINT)
                .is_err()
        );
        ring.begin_present_interval(51, false);
        ring.slots[0].state = SlotState::Recorded {
            capture_id: 76,
            present_seq: 50,
            list: 1,
            draws_before_in_list: Some(1),
        };
        assert!(
            ring.rebind_authoring_source(&new, identity(29), DXGI_FORMAT_D32_FLOAT_S8X24_UINT)
                .is_err()
        );
        ring.slots[0].state = SlotState::Free; // fixture had no real GPU submission
        let retired = ring
            .rebind_authoring_source(&new, identity(29), DXGI_FORMAT_D32_FLOAT_S8X24_UINT)
            .unwrap();
        assert_eq!(retired.as_raw(), old.as_raw());
        assert_eq!(ring.source.as_raw(), new.as_raw());
        assert_eq!(ring.source_identity(), identity(29));
        assert_eq!(
            ring.slots
                .iter()
                .map(|s| s.readback.as_raw())
                .collect::<Vec<_>>(),
            storage
        );
        assert_eq!(ring.fence.as_raw(), fence);
        assert_eq!(
            (
                ring.next_capture_id,
                ring.next_fence,
                ring.dropped_pool_full,
                ring.queue_identity
            ),
            (77, 101, 3, Some(123))
        );
        assert!(
            ring.rebind_authoring_source(
                &texture(32),
                identity(30),
                DXGI_FORMAT_D32_FLOAT_S8X24_UINT
            )
            .is_err()
        );
        assert_eq!(ring.source_identity(), identity(29));
        ring.invalidate("fixture failed session");
        assert!(
            ring.rebind_authoring_source(&old, identity(6), DXGI_FORMAT_D32_FLOAT_S8X24_UINT)
                .is_err()
        );
        drop(retired); // backend must do this only outside its observation/capture/tracker locks
    }

    fn source_desc() -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: 1920,
            Height: 1080,
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

    #[test]
    fn only_single_sample_d32_compatible_resource_view_pairs_are_admitted() {
        assert_eq!(
            validate_source_desc(&source_desc(), DXGI_FORMAT_D32_FLOAT),
            Ok(())
        );

        let mut typeless = source_desc();
        typeless.Format = DXGI_FORMAT_R32_TYPELESS;
        assert_eq!(
            validate_source_desc(&typeless, DXGI_FORMAT_D32_FLOAT),
            Ok(())
        );

        let mut d32s8 = source_desc();
        d32s8.Format = DXGI_FORMAT_R32G8X24_TYPELESS;
        assert_eq!(
            validate_source_desc(&d32s8, DXGI_FORMAT_D32_FLOAT_S8X24_UINT),
            Ok(())
        );
        assert!(
            validate_source_desc(&d32s8, DXGI_FORMAT_D32_FLOAT)
                .unwrap_err()
                .contains("resource/view pair")
        );

        assert!(
            validate_source_desc(&source_desc(), DXGI_FORMAT_R32_TYPELESS)
                .unwrap_err()
                .contains("resource/view pair")
        );

        let mut msaa = source_desc();
        msaa.SampleDesc.Count = 4;
        assert!(
            validate_source_desc(&msaa, DXGI_FORMAT_D32_FLOAT)
                .unwrap_err()
                .contains("MSAA")
        );
    }

    #[test]
    fn plane_zero_extraction_is_tight_and_excludes_distinct_stencil_bytes() {
        const WIDTH: usize = 3;
        const HEIGHT: usize = 2;
        const OFFSET: usize = 512;
        const SOURCE_ELEMENT_BYTES: usize = 8;
        const SOURCE_ROW_BYTES: usize = WIDTH * SOURCE_ELEMENT_BYTES;
        const OUTPUT_ROW_BYTES: usize = WIDTH * 4;
        const ROW_PITCH: usize = 256;
        let depth_bits = [
            0x3d80_0000_u32,
            0x3e80_0000,
            0x3f00_0000,
            0x3f20_0000,
            0x3f40_0000,
            0x3f60_0000,
        ];
        let mut mapped = vec![0xcc; OFFSET + ROW_PITCH * HEIGHT + 64];
        for (index, bits) in depth_bits.iter().enumerate() {
            let row = index / WIDTH;
            let column = index % WIDTH;
            let start = OFFSET + row * ROW_PITCH + column * SOURCE_ELEMENT_BYTES;
            mapped[start..start + 4].copy_from_slice(&bits.to_le_bytes());
            let distinct_stencil_x = 0x5a00_0000_u32 | u32::try_from(index).unwrap();
            mapped[start + 4..start + 8].copy_from_slice(&distinct_stencil_x.to_le_bytes());
        }
        let layout = DepthPlaneLayout {
            footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: OFFSET as u64,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS,
                    Width: WIDTH as u32,
                    Height: HEIGHT as u32,
                    Depth: 1,
                    RowPitch: ROW_PITCH as u32,
                },
            },
            rows: HEIGHT,
            source_row_bytes: SOURCE_ROW_BYTES,
            source_element_bytes: SOURCE_ELEMENT_BYTES,
            output_row_bytes: OUTPUT_ROW_BYTES,
            total_bytes: mapped.len() as u64,
            encoding: DepthPlaneEncoding::Float32,
        };

        let dense = extract_tight_depth_plane(&mapped, layout).unwrap();
        let expected: Vec<_> = depth_bits.into_iter().flat_map(u32::to_le_bytes).collect();
        assert_eq!(dense, expected);
        assert_eq!(dense.len(), OUTPUT_ROW_BYTES * HEIGHT);
        assert!(
            !dense
                .windows(4)
                .any(|word| word == 0x5a00_0000_u32.to_le_bytes())
        );
    }

    #[test]
    fn d24_plane_normalizes_integer_bits_and_rejects_reserved_bits() {
        let layout = DepthPlaneLayout {
            footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 4,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: DXGI_FORMAT_R32_TYPELESS,
                    RowPitch: 256,
                    ..Default::default()
                },
            },
            rows: 2,
            source_row_bytes: 8,
            source_element_bytes: 4,
            output_row_bytes: 8,
            total_bytes: 268,
            encoding: DepthPlaneEncoding::Unorm24,
        };
        let mut bytes = vec![0xcc; 268];
        for (offset, word) in [(4, 0u32), (8, 0xffffff), (260, 0x800000), (264, 1)] {
            bytes[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        }
        let dense = extract_tight_depth_plane(&bytes, layout).unwrap();
        let actual: Vec<_> = dense
            .chunks_exact(4)
            .map(|word| f32::from_le_bytes(word.try_into().unwrap()))
            .collect();
        assert_eq!(
            actual,
            [0.0, 1.0, 0x800000 as f32 / 16_777_215.0, 1.0 / 16_777_215.0]
        );
        bytes[7] = 0x80;
        assert!(
            extract_tight_depth_plane(&bytes, layout)
                .unwrap_err()
                .contains("reserved X8")
        );
        let mut desc = source_desc();
        desc.Format = DXGI_FORMAT_R24G8_TYPELESS;
        assert!(validate_source_desc(&desc, DXGI_FORMAT_D24_UNORM_S8_UINT).is_ok());
        assert!(validate_source_desc(&desc, DXGI_FORMAT_D32_FLOAT).is_err());
    }

    #[test]
    fn plane_zero_extraction_rejects_short_or_overwide_layouts() {
        let mut layout = DepthPlaneLayout {
            footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: 0,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    RowPitch: 8,
                    ..Default::default()
                },
            },
            rows: 2,
            source_row_bytes: 8,
            source_element_bytes: 4,
            output_row_bytes: 8,
            total_bytes: 16,
            encoding: DepthPlaneEncoding::Float32,
        };
        assert!(extract_tight_depth_plane(&[0; 15], layout).is_err());
        layout.source_row_bytes = 9;
        assert!(extract_tight_depth_plane(&[0; 18], layout).is_err());
    }
}

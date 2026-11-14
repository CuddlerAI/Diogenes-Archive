//! GPU-only selected-Present candidates on the swapchain's exact DIRECT queue.
//! App lists only snapshot depth. Pooling uses private lists; only Select copies
//! a candidate to READBACK. All fence polling is non-blocking, including priming.
//! Completed GPU slots stay leased while one bounded CPU worker copies their bytes.
//! Present only queues work/accepts matching ordered results; Stop includes both
//! queued copies and completed frames awaiting delivery in its in-flight accounting.

use std::mem::ManuallyDrop;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use depth_lab::decode::{DepthFormat, ZConvention};
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, s};

use crate::d3d12_capture::{GpuDepthSnapshot, create_buffer};
use crate::readback::{CandidateGpuAllocationStats, RawDepthFrame};

const POOL_SHADER: &str = r#"
ByteAddressBuffer source : register(t0);
RWByteAddressBuffer destination : register(u0);
cbuffer Geometry : register(b0) {
    uint src_w, src_h, dst_w, dst_h;
    uint row_pitch, element_bytes, take_max, encoding;
};
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= dst_w || id.y >= dst_h) return;
    // Native raw D32 diagnostics must preserve all original bits, including NaNs
    // and signed zero. No reduction or floating-point operation is needed here.
    if (src_w == dst_w && src_h == dst_h && encoding == 0) {
        destination.Store((id.y * dst_w + id.x) * 4, source.Load(id.y * row_pitch + id.x * element_bytes));
        return;
    }
    uint x0 = id.x * src_w / dst_w;
    uint y0 = id.y * src_h / dst_h;
    uint x1 = max(x0 + 1, min(src_w, ((id.x + 1) * src_w + dst_w - 1) / dst_w));
    uint y1 = max(y0 + 1, min(src_h, ((id.y + 1) * src_h + dst_h - 1) / dst_h));
    float acc = asfloat(0x7fc00000);
    for (uint y = y0; y < y1; ++y) {
        for (uint x = x0; x < x1; ++x) {
            uint bits = source.Load(y * row_pitch + x * element_bytes);
            if (encoding == 1 && (bits & 0xff000000) != 0) {
                destination.Store((id.y * dst_w + id.x) * 4, 0x7fc00000);
                return;
            }
            float v = encoding == 1 ? float(bits & 0x00ffffff) / 16777215.0 : asfloat(bits);
            acc = take_max ? max(acc, v) : min(acc, v);
        }
    }
    destination.Store((id.y * dst_w + id.x) * 4, asuint(acc));
}
"#;

struct Commands {
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: u64,
    submitted_at: Option<Instant>,
}

struct SelectedReadback {
    resource: ID3D12Resource,
    pending: Option<(u64, u64, Instant)>, // Present, fence, submission time
    copying: bool,
}

struct CpuCopyJob {
    slot: usize,
    present_seq: u64,
    resource: ID3D12Resource,
    len: usize,
    queued_at: Instant,
    metrics_enabled: bool,
}

struct CpuCopyResult {
    slot: usize,
    present_seq: u64,
    bytes: Result<Vec<u8>, String>,
    timing: Option<crate::perf::ReadbackCopyRecord>,
}

struct CpuCopyWorker {
    requests: SyncSender<CpuCopyJob>,
    results: Receiver<CpuCopyResult>,
}

impl CpuCopyWorker {
    fn new(slots: usize) -> Result<Self, String> {
        if slots == 0 {
            return Err("D3D12 CPU readback worker requires at least one leased slot".into());
        }
        let (requests, jobs) = sync_channel::<CpuCopyJob>(slots);
        let (replies, results) = sync_channel(slots);
        std::thread::Builder::new()
            .name("dm-d3d12-readback".into())
            .spawn(move || {
                crate::probe::set_current_thread_lossless_writer_priority();
                while let Ok(job) = jobs.recv() {
                    let result = copy_ready_resource(job);
                    // At most one result per leased slot can exist. Closing the owner also
                    // closes this channel; stop without touching the next generation.
                    if replies.send(result).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| format!("spawn D3D12 CPU readback worker: {error}"))?;
        // The detached worker owns strong references only to GPU-completed resources.
        // Dropping these channels ends it without joining/blocking a Present thread.
        Ok(Self { requests, results })
    }
}

fn copy_ready_resource(job: CpuCopyJob) -> CpuCopyResult {
    let mut timing = None;
    let bytes = (|| {
        let started = Instant::now();
        let queue_wait_ns = job.queued_at.elapsed().as_nanos() as u64;
        let mut start_qpc = 0;
        if job.metrics_enabled {
            unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut start_qpc) }
                .map_err(|error| format!("readback worker QPC failed: {error}"))?;
        }
        let range = D3D12_RANGE {
            Begin: 0,
            End: job.len,
        };
        let mut mapped = std::ptr::null_mut();
        let map_started = Instant::now();
        unsafe { job.resource.Map(0, Some(&range), Some(&mut mapped)) }.map_err(gpu_error)?;
        let map_ns = map_started.elapsed().as_nanos() as u64;
        if mapped.is_null() {
            unsafe {
                job.resource
                    .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }))
            };
            return Err("selected D3D12 worker Map returned null".into());
        }
        let copy_started = Instant::now();
        // SAFETY: enqueue occurs only after the exact GPU fence completes. The slot
        // remains leased until this owned result is consumed; Map/Unmap stay on this
        // worker and no mapped pointer is sent to any other thread.
        let bytes = unsafe { crate::readback::copy_mapped_bytes(mapped.cast::<u8>(), job.len) };
        let copy_ns = copy_started.elapsed().as_nanos() as u64;
        unsafe {
            job.resource
                .Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }))
        };
        if job.metrics_enabled {
            timing = Some(crate::perf::ReadbackCopyRecord {
                present_seq: job.present_seq,
                start_qpc,
                duration_ns: started.elapsed().as_nanos() as u64,
                map_ns,
                copy_ns,
                queue_wait_ns,
            });
        }
        Ok(bytes)
    })();
    CpuCopyResult {
        slot: job.slot,
        present_seq: job.present_seq,
        bytes,
        timing,
    }
}

pub struct D3d12CandidateRing {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    candidates: Vec<ID3D12Resource>,
    readbacks: Vec<SelectedReadback>,
    cpu_copy: CpuCopyWorker,
    completed_frames: Vec<RawDepthFrame>,
    commands: Vec<Commands>,
    fence: ID3D12Fence,
    next_fence: u64,
    prime: Option<(ID3D12Resource, u64)>,
    width: u32,
    height: u32,
    conv: ZConvention,
    stats: CandidateGpuAllocationStats,
    completed: u64,
    fatal: Option<String>,
}

impl Drop for D3d12CandidateRing {
    fn drop(&mut self) {
        if self.gpu_in_flight() == 0 {
            return;
        }
        // Execute with an unknown/failed Signal must never free GPU-visible resources.
        std::mem::forget(self.device.clone());
        std::mem::forget(self.queue.clone());
        std::mem::forget(self.root.clone());
        std::mem::forget(self.pipeline.clone());
        std::mem::forget(self.fence.clone());
        for resource in &self.candidates {
            std::mem::forget(resource.clone());
        }
        for slot in &self.readbacks {
            std::mem::forget(slot.resource.clone());
        }
        for commands in &self.commands {
            std::mem::forget(commands.allocator.clone());
            std::mem::forget(commands.list.clone());
        }
        if let Some((upload, _)) = &self.prime {
            std::mem::forget(upload.clone());
        }
    }
}

impl D3d12CandidateRing {
    pub fn new(
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        bootstrap: &depth_lab::SelectedPresentFeedbackBootstrap,
        readback_slots: usize,
        snapshot_bytes: u64,
        conv: ZConvention,
    ) -> Result<Self, String> {
        if unsafe { queue.GetDesc() }.Type != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return Err("D3D12 candidate queue is not DIRECT".into());
        }
        let mut queue_device = None;
        unsafe { queue.GetDevice(&mut queue_device) }.map_err(gpu_error)?;
        let queue_device: ID3D12Device = queue_device.ok_or("candidate queue device missing")?;
        if queue_device.as_raw() != device.as_raw() {
            return Err("D3D12 candidate queue/device mismatch".into());
        }
        let capacity = bootstrap.ring_capacity;
        let bytes = candidate_bytes(bootstrap.width, bootstrap.height)?;
        let estimated_bytes = allocation_bytes(bytes, capacity, readback_slots, snapshot_bytes)?;
        let before = query_memory(device)?;
        let headroom = before.Budget.saturating_sub(before.CurrentUsage);
        validate_budget(
            estimated_bytes,
            bootstrap.vram_limit_bytes,
            bootstrap.vram_reserve_bytes,
            headroom,
        )?;
        let (root, pipeline) = create_pipeline(device)?;
        let candidates = (0..capacity)
            .map(|_| create_buffer(device, bytes, true, true))
            .collect::<Result<Vec<_>, _>>()?;
        let readbacks = (0..readback_slots)
            .map(|_| {
                Ok(SelectedReadback {
                    resource: create_buffer(device, bytes, false, false)?,
                    pending: None,
                    copying: false,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        // Two submissions per Present at most (pool + Select), plus the producer pipeline.
        let commands = (0..readback_slots + crate::d3d12_capture::MAX_CAPTURE_SLOTS + 1)
            .map(|_| create_commands(device))
            .collect::<Result<Vec<_>, _>>()?;
        let fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(gpu_error)?;
        let mut ring = Self {
            device: device.clone(),
            queue: queue.clone(),
            root,
            pipeline,
            candidates,
            readbacks,
            cpu_copy: CpuCopyWorker::new(readback_slots)?,
            completed_frames: Vec::new(),
            commands,
            fence,
            next_fence: 1,
            prime: None,
            width: bootstrap.width,
            height: bootstrap.height,
            conv,
            completed: 0,
            fatal: None,
            stats: CandidateGpuAllocationStats {
                capacity,
                texture_bytes: bytes,
                estimated_bytes,
                configured_limit_bytes: bootstrap.vram_limit_bytes,
                configured_reserve_bytes: bootstrap.vram_reserve_bytes,
                budget_before: before.Budget,
                usage_before: before.CurrentUsage,
                headroom_before: headroom,
                budget_after: 0,
                usage_after: 0,
                headroom_after: 0,
                actual_usage_delta: 0,
                budget_at_flush: None,
                usage_at_flush: None,
                headroom_at_flush: None,
            },
        };
        ring.prime_allocations()?;
        Ok(ring)
    }

    fn prime_allocations(&mut self) -> Result<(), String> {
        let bytes = self.stats.texture_bytes;
        let mut desc = unsafe { self.candidates[0].GetDesc() };
        desc.Flags = D3D12_RESOURCE_FLAG_NONE;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
            ..Default::default()
        };
        let mut upload = None;
        unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut upload,
            )
        }
        .map_err(gpu_error)?;
        let upload: ID3D12Resource = upload.ok_or("candidate prime upload missing")?;
        let mut mapped = std::ptr::null_mut();
        let empty = D3D12_RANGE { Begin: 0, End: 0 };
        unsafe { upload.Map(0, Some(&empty), Some(&mut mapped)) }.map_err(gpu_error)?;
        if mapped.is_null() {
            return Err("candidate prime Map returned null".into());
        }
        unsafe {
            std::ptr::write_bytes(mapped.cast::<u8>(), 0, bytes as usize);
            upload.Unmap(0, None);
        }
        let index = self.begin_commands()?;
        let list = &self.commands[index].list;
        for target in &self.candidates {
            unsafe { list.CopyBufferRegion(target, 0, &upload, 0, bytes) };
            transition(
                list,
                target,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
        }
        for target in &self.readbacks {
            unsafe { list.CopyBufferRegion(&target.resource, 0, &upload, 0, bytes) };
        }
        // Own upload before Execute: Signal failure also needs to retain it.
        self.prime = Some((upload, u64::MAX));
        let fence = self.submit(index)?;
        self.prime.as_mut().expect("prime upload retained").1 = fence;
        Ok(())
    }

    /// Allocation proof completes on later Presents; never wait for the GPU here.
    pub fn ready(&mut self) -> Result<bool, String> {
        let completed = self.poll()?;
        if let Some((_, fence)) = self.prime.as_ref() {
            if completed < *fence {
                return Ok(false);
            }
            let after = query_memory(&self.device)?;
            let headroom = after.Budget.saturating_sub(after.CurrentUsage);
            let delta = after.CurrentUsage.saturating_sub(self.stats.usage_before);
            if headroom < self.stats.configured_reserve_bytes
                || delta > self.stats.configured_limit_bytes
            {
                return Err(
                    "D3D12 candidate allocation exceeded DXGI budget/reserve after priming".into(),
                );
            }
            self.stats.budget_after = after.Budget;
            self.stats.usage_after = after.CurrentUsage;
            self.stats.headroom_after = headroom;
            self.stats.actual_usage_delta = delta;
            self.prime = None;
        }
        Ok(true)
    }

    pub fn materialize(
        &mut self,
        snapshot: &GpuDepthSnapshot,
        slot: usize,
    ) -> Result<(ID3D12Fence, u64), String> {
        let _perf = crate::perf::scope(crate::perf::Section::CandidateMaterialize);
        if !self.ready()? {
            return Err("D3D12 candidate allocation prime is still pending".into());
        }
        if snapshot.queue != self.queue.as_raw() as usize {
            return Err("D3D12 depth snapshot and RGB swapchain use different queues".into());
        }
        validate_geometry(
            snapshot.width,
            snapshot.height,
            self.width,
            self.height,
            snapshot.row_pitch,
            snapshot.element_bytes,
        )?;
        let target = self
            .candidates
            .get(slot)
            .ok_or("candidate slot out of bounds")?
            .clone();
        let index = self.begin_commands()?;
        let list = &self.commands[index].list;
        transition(
            list,
            &snapshot.resource,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        transition(
            list,
            &target,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let constants = [
            snapshot.width,
            snapshot.height,
            self.width,
            self.height,
            snapshot.row_pitch,
            snapshot.element_bytes,
            u32::from(self.conv == ZConvention::Reversed),
            snapshot.encoding as u32,
        ];
        unsafe {
            list.SetPipelineState(&self.pipeline);
            list.SetComputeRootSignature(&self.root);
            list.SetComputeRootShaderResourceView(0, snapshot.resource.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(1, target.GetGPUVirtualAddress());
            list.SetComputeRoot32BitConstants(2, 8, constants.as_ptr().cast(), 0);
            list.Dispatch(self.width.div_ceil(8), self.height.div_ceil(8), 1);
        }
        transition(
            list,
            &target,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        transition(
            list,
            &snapshot.resource,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let fence = self.submit(index)?;
        Ok((self.fence.clone(), fence))
    }

    pub fn select(&mut self, candidate: usize, present_seq: u64) -> Result<bool, String> {
        let _perf = crate::perf::scope(crate::perf::Section::SelectedReadback);
        self.poll()?;
        // A worker can finish after this Present's earlier drain. Reclaim completed
        // leases before deciding that the bounded GPU staging ring is full.
        self.collect_cpu_results()?;
        let Some(slot) = self
            .readbacks
            .iter()
            .position(|slot| slot.pending.is_none())
        else {
            return Ok(false);
        };
        let source = self
            .candidates
            .get(candidate)
            .ok_or("selected candidate slot out of bounds")?
            .clone();
        let index = self.begin_commands()?;
        unsafe {
            self.commands[index].list.CopyBufferRegion(
                &self.readbacks[slot].resource,
                0,
                &source,
                0,
                self.stats.texture_bytes,
            )
        };
        self.readbacks[slot].pending = Some((present_seq, u64::MAX, Instant::now()));
        let fence = self.submit(index)?;
        self.readbacks[slot]
            .pending
            .as_mut()
            .expect("pending readback retained")
            .1 = fence;
        Ok(true)
    }

    fn collect_cpu_results(&mut self) -> Result<(), String> {
        loop {
            let result = match self.cpu_copy.results.try_recv() {
                Ok(result) => result,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    return Err("D3D12 CPU readback worker exited".into());
                }
            };
            let oldest = self
                .readbacks
                .iter()
                .filter_map(|slot| slot.pending.map(|p| p.0))
                .min();
            if self.completed_frames.len() >= self.readbacks.len() {
                return Err("D3D12 completed CPU frame queue exceeded its slot bound".into());
            }
            let slot = self
                .readbacks
                .get_mut(result.slot)
                .ok_or("CPU readback slot out of bounds")?;
            if !slot.copying
                || slot.pending.map(|p| p.0) != Some(result.present_seq)
                || oldest != Some(result.present_seq)
            {
                return Err("D3D12 CPU readback result identity/order mismatch".into());
            }
            let bytes = result.bytes?;
            if bytes.len() != self.stats.texture_bytes as usize {
                return Err("D3D12 CPU readback returned an incomplete frame".into());
            }
            if let Some(timing) = result.timing {
                // Only the live owner records metrics. An aborted generation's worker
                // can finish safely, but its detached reply cannot enter a newer ring.
                crate::perf::record_readback_copy(timing);
            }
            self.completed_frames.push(RawDepthFrame {
                present_seq: result.present_seq,
                src_width: self.width as usize,
                src_height: self.height as usize,
                dst_width: self.width as usize,
                dst_height: self.height as usize,
                row_pitch: self.width as usize * 4,
                format: DepthFormat::D32Float,
                conv: self.conv,
                bytes,
                pooled: true,
                verify_pooled: None,
                verify_row_pitch: 0,
            });
            slot.pending = None;
            slot.copying = false;
            self.completed += 1;
        }
        Ok(())
    }

    pub fn drain_ready(&mut self) -> Result<Vec<RawDepthFrame>, String> {
        let completed = self.poll()?;
        self.collect_cpu_results()?;
        // A slot stays unavailable until its worker result has been accepted above.
        // Queue in Present order even when physical slot indices have wrapped.
        let mut pending: Vec<_> = self
            .readbacks
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| slot.pending.map(|p| (p.0, index)))
            .collect();
        pending.sort_unstable();
        for (present_seq, index) in pending {
            let slot = &mut self.readbacks[index];
            let (_, fence, submitted) = slot.pending.expect("pending slot retained");
            if submitted.elapsed() > Duration::from_secs(2) {
                return Err("selected D3D12 GPU/CPU readback timed out".into());
            }
            if slot.copying || completed < fence {
                continue;
            }
            self.cpu_copy
                .requests
                .try_send(CpuCopyJob {
                    slot: index,
                    present_seq,
                    resource: slot.resource.clone(),
                    len: self.stats.texture_bytes as usize,
                    queued_at: Instant::now(),
                    metrics_enabled: crate::perf::enabled(),
                })
                .map_err(|error| format!("enqueue D3D12 CPU readback: {error}"))?;
            slot.copying = true;
        }
        Ok(std::mem::take(&mut self.completed_frames))
    }

    pub fn completed(&self) -> u64 {
        self.completed
    }
    pub fn stats(&self) -> CandidateGpuAllocationStats {
        self.stats
    }
    pub fn in_flight(&self) -> usize {
        self.gpu_in_flight()
            + self.completed_frames.len()
            + self
                .readbacks
                .iter()
                .filter(|slot| slot.pending.is_some())
                .count()
    }
    fn gpu_in_flight(&self) -> usize {
        let completed = unsafe { self.fence.GetCompletedValue() };
        self.commands
            .iter()
            .filter(|slot| slot.fence != 0 && (completed == u64::MAX || slot.fence > completed))
            .count()
    }
    pub fn refresh_final_stats(&mut self) -> Result<(), String> {
        if !self.ready()? {
            return Err("D3D12 allocation prime unresolved at Flush".into());
        }
        let memory = query_memory(&self.device)?;
        self.stats.budget_at_flush = Some(memory.Budget);
        self.stats.usage_at_flush = Some(memory.CurrentUsage);
        self.stats.headroom_at_flush = Some(memory.Budget.saturating_sub(memory.CurrentUsage));
        Ok(())
    }

    fn poll(&self) -> Result<u64, String> {
        if let Some(error) = &self.fatal {
            return Err(error.clone());
        }
        let value = unsafe { self.fence.GetCompletedValue() };
        if value == u64::MAX {
            return Err("D3D12 candidate device removed".into());
        }
        if self.commands.iter().any(|slot| {
            slot.fence > value
                && slot
                    .submitted_at
                    .is_some_and(|time| time.elapsed() > Duration::from_secs(2))
        }) {
            return Err("D3D12 candidate command fence timed out".into());
        }
        Ok(value)
    }
    fn begin_commands(&mut self) -> Result<usize, String> {
        let completed = self.poll()?;
        let index = self
            .commands
            .iter()
            .position(|slot| slot.fence <= completed)
            .ok_or("D3D12 private command pool is full")?;
        let slot = &self.commands[index];
        unsafe {
            slot.allocator.Reset().map_err(gpu_error)?;
            slot.list
                .Reset(&slot.allocator, None::<&ID3D12PipelineState>)
                .map_err(gpu_error)?;
        }
        Ok(index)
    }
    fn submit(&mut self, index: usize) -> Result<u64, String> {
        let value = self.next_fence;
        self.next_fence = value
            .checked_add(1)
            .filter(|v| *v < u64::MAX)
            .ok_or("candidate fence overflow")?;
        let commands = &mut self.commands[index];
        unsafe { commands.list.Close() }.map_err(gpu_error)?;
        let base: ID3D12CommandList = commands.list.cast().map_err(gpu_error)?;
        commands.fence = u64::MAX;
        commands.submitted_at = Some(Instant::now());
        unsafe { self.queue.ExecuteCommandLists(&[Some(base)]) };
        if let Err(error) = unsafe { self.queue.Signal(&self.fence, value) } {
            let error = gpu_error(error);
            self.fatal = Some(error.clone());
            return Err(error);
        }
        commands.fence = value;
        Ok(value)
    }
}

fn candidate_bytes(width: u32, height: u32) -> Result<u64, String> {
    if width == 0 || height == 0 || width > 8192 || height > 8192 {
        return Err("invalid D3D12 candidate geometry".into());
    }
    Ok(u64::from(width) * u64::from(height) * 4)
}

fn allocation_bytes(
    bytes: u64,
    capacity: u32,
    readback_slots: usize,
    snapshot_bytes: u64,
) -> Result<u64, String> {
    if capacity == 0 || capacity > 1024 || !(4..=16).contains(&readback_slots) {
        return Err("invalid D3D12 candidate/readback capacity".into());
    }
    // Include committed-resource alignment and the temporary upload used by priming.
    let aligned = bytes
        .checked_add(65535)
        .map(|n| n / 65536 * 65536)
        .ok_or("candidate alignment overflow")?;
    aligned
        .checked_mul(u64::from(capacity) + readback_slots as u64 + 1)
        .and_then(|n| n.checked_add(snapshot_bytes))
        .ok_or_else(|| "candidate allocation overflow".into())
}

fn validate_budget(bytes: u64, limit: u64, reserve: u64, headroom: u64) -> Result<(), String> {
    if bytes > limit
        || bytes
            .checked_add(reserve)
            .is_none_or(|required| required > headroom)
    {
        return Err(format!(
            "D3D12 candidate allocation {bytes} exceeds limit {limit} or DXGI headroom {headroom} with reserve {reserve}"
        ));
    }
    Ok(())
}

fn validate_geometry(
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    pitch: u32,
    element: u32,
) -> Result<(), String> {
    candidate_bytes(sw, sh)?;
    candidate_bytes(dw, dh)?;
    if !matches!(element, 4 | 8) || pitch < sw.saturating_mul(element) || !pitch.is_multiple_of(4) {
        return Err("invalid D3D12 snapshot plane layout".into());
    }
    Ok(())
}

fn query_memory(device: &ID3D12Device) -> Result<DXGI_QUERY_VIDEO_MEMORY_INFO, String> {
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.map_err(gpu_error)?;
    let adapter: IDXGIAdapter3 =
        unsafe { factory.EnumAdapterByLuid(device.GetAdapterLuid()) }.map_err(gpu_error)?;
    let mut memory = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    unsafe { adapter.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut memory) }
        .map_err(gpu_error)?;
    Ok(memory)
}

/// Conservatively reserve the largest admitted plane stride before allocating source snapshots.
pub fn preflight_snapshot_allocation(
    device: &ID3D12Device,
    bootstrap: &depth_lab::SelectedPresentFeedbackBootstrap,
    width: u32,
    height: u32,
    slots: usize,
) -> Result<(), String> {
    candidate_bytes(width, height)?;
    let source_pitch = (u64::from(width) * 8).div_ceil(256) * 256;
    let source_bytes = (source_pitch * u64::from(height)).div_ceil(65536) * 65536 * slots as u64;
    let total = allocation_bytes(
        candidate_bytes(bootstrap.width, bootstrap.height)?,
        bootstrap.ring_capacity,
        slots,
        source_bytes,
    )?;
    let memory = query_memory(device)?;
    validate_budget(
        total,
        bootstrap.vram_limit_bytes,
        bootstrap.vram_reserve_bytes,
        memory.Budget.saturating_sub(memory.CurrentUsage),
    )
}

fn create_commands(device: &ID3D12Device) -> Result<Commands, String> {
    let allocator = unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
        .map_err(gpu_error)?;
    let list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .map_err(gpu_error)?;
    unsafe { list.Close() }.map_err(gpu_error)?;
    Ok(Commands {
        allocator,
        list,
        fence: 0,
        submitted_at: None,
    })
}

fn create_pipeline(
    device: &ID3D12Device,
) -> Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
    let params = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 8,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: 3,
        pParameters: params.as_ptr(),
        ..Default::default()
    };
    let mut signature: Option<ID3DBlob> = None;
    let mut errors = None;
    unsafe {
        D3D12SerializeRootSignature(
            &desc,
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut signature,
            Some(&mut errors),
        )
    }
    .map_err(gpu_error)?;
    let signature = signature.ok_or("empty D3D12 root signature")?;
    let root: ID3D12RootSignature =
        unsafe { device.CreateRootSignature(0, blob_bytes(&signature)) }.map_err(gpu_error)?;
    let mut shader = None;
    unsafe {
        D3DCompile(
            POOL_SHADER.as_ptr().cast(),
            POOL_SHADER.len(),
            None,
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_IEEE_STRICTNESS | D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut shader,
            Some(&mut errors),
        )
    }
    .map_err(|error| {
        format!(
            "D3D12 pool shader: {error}; {}",
            errors
                .as_ref()
                .map_or_else(String::new, |blob| String::from_utf8_lossy(blob_bytes(
                    blob
                ))
                .into_owned())
        )
    })?;
    let shader = shader.ok_or("empty D3D12 pool shader")?;
    let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        CS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: unsafe { shader.GetBufferPointer() },
            BytecodeLength: unsafe { shader.GetBufferSize() },
        },
        ..Default::default()
    };
    let result = unsafe { device.CreateComputePipelineState(&desc) }.map_err(gpu_error);
    unsafe { ManuallyDrop::drop(&mut desc.pRootSignature) };
    Ok((root, result?))
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe {
        std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
    }
}
fn gpu_error(error: windows::core::Error) -> String {
    format!("D3D12 candidate GPU call failed: {error}")
}

fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
        ..Default::default()
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use depth_lab::selected_present_candidate_ring::{
        CandidateRingConfig, CandidateRingState, SelectPlan,
    };

    pub(crate) fn warp_device() -> (ID3D12Device, ID3D12CommandQueue) {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.unwrap();
        let adapter: IDXGIAdapter = unsafe { factory.EnumWarpAdapter() }.unwrap();
        let mut device = None;
        unsafe { D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device) }.unwrap();
        let device: ID3D12Device = device.unwrap();
        let queue = unsafe {
            device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
        }
        .unwrap();
        (device, queue)
    }

    pub(crate) fn test_bootstrap() -> depth_lab::SelectedPresentFeedbackBootstrap {
        depth_lab::SelectedPresentFeedbackBootstrap {
            client_process_id: 1,
            nonce: "warp-test".into(),
            resource_epoch: 1,
            queue_capacity: 32,
            ring_capacity: 4,
            max_feedback_lag_presents: 3,
            guard_slots: 1,
            width: 3,
            height: 2,
            bytes_per_pixel: 4,
            vram_limit_bytes: 64 * 1024 * 1024,
            vram_reserve_bytes: 1024 * 1024,
        }
    }

    pub(crate) fn wait_until(mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(Instant::now() < deadline, "WARP GPU test timed out");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    pub(crate) fn filled_snapshot(
        device: &ID3D12Device,
        queue: &ID3D12CommandQueue,
        words: &[f32],
        element_bytes: u32,
        seq: u64,
    ) -> GpuDepthSnapshot {
        let (width, height, pitch) = (7, 5, 256);
        let bytes = u64::from(pitch * height);
        let resource = create_buffer(device, bytes, true, false).unwrap();
        let desc = unsafe { resource.GetDesc() };
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
            ..Default::default()
        };
        let mut upload = None;
        unsafe {
            device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut upload,
            )
        }
        .unwrap();
        let upload: ID3D12Resource = upload.unwrap();
        let mut mapped = std::ptr::null_mut();
        unsafe { upload.Map(0, None, Some(&mut mapped)) }.unwrap();
        unsafe { std::ptr::write_bytes(mapped.cast::<u8>(), 0xcc, bytes as usize) };
        for (i, value) in words.iter().enumerate() {
            let offset =
                i / width as usize * pitch as usize + i % width as usize * element_bytes as usize;
            unsafe {
                std::ptr::copy_nonoverlapping(
                    value.to_le_bytes().as_ptr(),
                    mapped.cast::<u8>().add(offset),
                    4,
                )
            };
        }
        unsafe { upload.Unmap(0, None) };
        let commands = create_commands(device).unwrap();
        unsafe {
            commands
                .list
                .Reset(&commands.allocator, None::<&ID3D12PipelineState>)
                .unwrap();
            commands
                .list
                .CopyBufferRegion(&resource, 0, &upload, 0, bytes);
            commands.list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(commands.list.cast().unwrap())]);
        }
        let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.unwrap();
        unsafe { queue.Signal(&fence, 1) }.unwrap();
        wait_until(|| unsafe { fence.GetCompletedValue() } >= 1);
        GpuDepthSnapshot {
            slot: 0,
            capture_id: seq,
            present_seq: seq,
            queue: queue.as_raw() as usize,
            resource,
            width,
            height,
            row_pitch: pitch,
            element_bytes,
            encoding: crate::d3d12_capture::DepthPlaneEncoding::Float32,
        }
    }

    #[test]
    fn warp_cpu_copy_lease_blocks_reuse_until_reply_and_survives_candidate_reuse() {
        let (device, queue) = warp_device();
        let mut gpu = D3d12CandidateRing::new(
            &device,
            &queue,
            &test_bootstrap(),
            4,
            0,
            ZConvention::Reversed,
        )
        .unwrap();
        wait_until(|| gpu.ready().unwrap());
        let (requests, jobs) = sync_channel(4);
        let (replies, results) = sync_channel(4);
        gpu.cpu_copy = CpuCopyWorker { requests, results };
        let source = filled_snapshot(&device, &queue, &[0.25; 35], 4, 1);
        gpu.materialize(&source, 0).unwrap();
        for seq in 1..=4 {
            assert!(gpu.select(0, seq).unwrap());
        }
        wait_until(|| gpu.poll().unwrap() >= gpu.readbacks[3].pending.unwrap().1);
        assert!(gpu.drain_ready().unwrap().is_empty());
        let job = jobs.try_recv().unwrap();
        assert!(gpu.readbacks[0].copying);
        assert_eq!(gpu.completed(), 0);
        assert!(
            !gpu.select(0, 5).unwrap(),
            "CPU copies still own all staging slots"
        );
        let replacement = filled_snapshot(&device, &queue, &[0.75; 35], 4, 5);
        gpu.materialize(&replacement, 0).unwrap();
        // The CPU work can finish after drain_ready but before Select. Select must
        // reclaim the reply without copying/waiting and preserve its output for drain.
        replies.send(copy_ready_resource(job)).unwrap();
        assert!(gpu.select(0, 5).unwrap());
        assert_eq!(gpu.completed(), 1);
        assert!(
            gpu.in_flight() > 0,
            "buffered completion/new GPU work must hold Stop"
        );
        let frames = gpu.drain_ready().unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].present_seq, 1);
        assert!(frames[0].decode().unwrap().iter().all(|v| *v == 0.25));
        wait_until(|| gpu.poll().unwrap() >= gpu.readbacks[0].pending.unwrap().1);
        gpu.drain_ready().unwrap();
        for seq in 2..=5 {
            let job = jobs.try_recv().unwrap();
            assert_eq!(job.present_seq, seq);
            replies.send(copy_ready_resource(job)).unwrap();
        }
        let frames = gpu.drain_ready().unwrap();
        assert_eq!(
            frames.iter().map(|f| f.present_seq).collect::<Vec<_>>(),
            [2, 3, 4, 5]
        );
        for frame in &frames {
            let expected = if frame.present_seq == 5 { 0.75 } else { 0.25 };
            assert!(frame.decode().unwrap().iter().all(|v| *v == expected));
        }
        assert_eq!(gpu.in_flight(), 0);
    }

    #[test]
    fn warp_cpu_worker_replies_are_ordered_and_mismatches_fail_closed() {
        let (device, queue) = warp_device();
        let mut gpu = D3d12CandidateRing::new(
            &device,
            &queue,
            &test_bootstrap(),
            4,
            0,
            ZConvention::Reversed,
        )
        .unwrap();
        wait_until(|| gpu.ready().unwrap());
        let (requests, jobs) = sync_channel(2);
        let (replies, results) = sync_channel(2);
        gpu.cpu_copy = CpuCopyWorker { requests, results };
        // Model physical slot wrap independently of Present order.
        gpu.readbacks[0].pending = Some((20, 0, Instant::now()));
        gpu.readbacks[1].pending = Some((10, 0, Instant::now()));
        gpu.drain_ready().unwrap();
        let first = jobs.try_recv().unwrap();
        let second = jobs.try_recv().unwrap();
        assert_eq!((first.present_seq, second.present_seq), (10, 20));
        replies
            .send(CpuCopyResult {
                slot: first.slot,
                present_seq: 999,
                bytes: Ok(vec![0; 24]),
                timing: None,
            })
            .unwrap();
        assert!(
            gpu.drain_ready()
                .err()
                .unwrap()
                .contains("identity/order mismatch")
        );
        assert_eq!(gpu.completed(), 0);
        assert_eq!(gpu.readbacks[0].pending.map(|p| p.0), Some(20));
        assert_eq!(gpu.readbacks[1].pending.map(|p| p.0), Some(10));
    }

    #[test]
    fn warp_cpu_copy_never_queues_before_fence_and_worker_failure_is_explicit() {
        let (device, queue) = warp_device();
        let mut gpu = D3d12CandidateRing::new(
            &device,
            &queue,
            &test_bootstrap(),
            4,
            0,
            ZConvention::Reversed,
        )
        .unwrap();
        wait_until(|| gpu.ready().unwrap());
        let (requests, jobs) = sync_channel(1);
        let (replies, results) = sync_channel(1);
        gpu.cpu_copy = CpuCopyWorker { requests, results };
        gpu.readbacks[0].pending = Some((10, gpu.poll().unwrap() + 100, Instant::now()));
        assert!(gpu.drain_ready().unwrap().is_empty());
        assert!(matches!(jobs.try_recv(), Err(TryRecvError::Empty)));
        assert!(!gpu.readbacks[0].copying);
        gpu.readbacks[0].pending.as_mut().unwrap().1 = 0;
        gpu.drain_ready().unwrap();
        let job = jobs.try_recv().unwrap();
        replies
            .send(CpuCopyResult {
                slot: job.slot,
                present_seq: job.present_seq,
                bytes: Err("injected Map failure".into()),
                timing: None,
            })
            .unwrap();
        assert!(
            gpu.drain_ready()
                .err()
                .unwrap()
                .contains("injected Map failure")
        );
        assert_eq!(gpu.completed(), 0);
        drop(replies);
        assert!(gpu.drain_ready().err().unwrap().contains("worker exited"));
    }

    #[test]
    fn warp_aborted_owner_does_not_free_a_worker_lease_or_reuse_its_reply() {
        let (device, queue) = warp_device();
        let mut gpu = D3d12CandidateRing::new(
            &device,
            &queue,
            &test_bootstrap(),
            4,
            0,
            ZConvention::Reversed,
        )
        .unwrap();
        wait_until(|| gpu.ready().unwrap());
        let (requests, jobs) = sync_channel(1);
        let (replies, results) = sync_channel(1);
        gpu.cpu_copy = CpuCopyWorker { requests, results };
        let source = filled_snapshot(&device, &queue, &[0.125; 35], 4, 1);
        gpu.materialize(&source, 0).unwrap();
        gpu.select(0, 1).unwrap();
        wait_until(|| gpu.poll().unwrap() >= gpu.readbacks[0].pending.unwrap().1);
        gpu.drain_ready().unwrap();
        let job = jobs.try_recv().unwrap();
        drop(gpu);
        let reply = copy_ready_resource(job);
        assert!(
            reply
                .bytes
                .as_ref()
                .unwrap()
                .chunks_exact(4)
                .all(|b| f32::from_le_bytes(b.try_into().unwrap()) == 0.125)
        );
        assert!(
            replies.send(reply).is_err(),
            "aborted owner must discard its late completion"
        );
        let mut next = D3d12CandidateRing::new(
            &device,
            &queue,
            &test_bootstrap(),
            4,
            0,
            ZConvention::Reversed,
        )
        .unwrap();
        wait_until(|| next.ready().unwrap());
        assert!(next.drain_ready().unwrap().is_empty());
        assert_eq!(next.completed(), 0);
        assert_eq!(next.in_flight(), 0);
    }

    #[test]
    fn warp_native_d32_candidates_preserve_original_bits_and_row_pitch() {
        let (device, queue) = warp_device();
        let mut config = test_bootstrap();
        config.width = 7;
        config.height = 5;
        let bit_patterns = [
            0u32, 0x80000000, 0x7fc01234, 0x3f800000, 0x00000001, 0x3a123456, 0x7f800000,
        ];
        let words: Vec<_> = (0..35)
            .map(|i| f32::from_bits(bit_patterns[i % bit_patterns.len()]))
            .collect();
        for stride in [4, 8] {
            let mut gpu =
                D3d12CandidateRing::new(&device, &queue, &config, 4, 0, ZConvention::Reversed)
                    .unwrap();
            wait_until(|| gpu.ready().unwrap());
            let source = filled_snapshot(&device, &queue, &words, stride, 17);
            gpu.materialize(&source, 0).unwrap();
            assert!(gpu.select(0, 17).unwrap());
            let mut frames = vec![];
            wait_until(|| {
                frames.extend(gpu.drain_ready().unwrap());
                !frames.is_empty()
            });
            assert_eq!(frames.len(), 1);
            assert_eq!(frames[0].present_seq, 17);
            let expected: Vec<_> = words.iter().flat_map(|v| v.to_le_bytes()).collect();
            assert_eq!(
                frames[0].bytes, expected,
                "native D32 bits, stride {stride}"
            );
        }
    }

    #[test]
    fn warp_selected_pool_matches_cpu_and_preserves_old_frame_after_slot_reuse() {
        let (device, queue) = warp_device();
        for conv in [ZConvention::Traditional, ZConvention::Reversed] {
            for stride in [4, 8] {
                let mut gpu =
                    D3d12CandidateRing::new(&device, &queue, &test_bootstrap(), 4, 0, conv)
                        .unwrap();
                wait_until(|| gpu.ready().unwrap());
                let mut state = CandidateRingState::new(
                    CandidateRingConfig {
                        capacity: 4,
                        max_feedback_lag_presents: 3,
                        guard_slots: 1,
                    },
                    1,
                    1,
                )
                .unwrap();
                state.arm().unwrap();
                let words: Vec<_> = (0..35)
                    .map(|i| {
                        if i % 4 == 0 {
                            f32::NAN
                        } else {
                            i as f32 / 35.0
                        }
                    })
                    .collect();
                let first = filled_snapshot(&device, &queue, &words, stride, 1);
                let slot = state.materialize(1).unwrap();
                gpu.materialize(&first, slot).unwrap();
                let SelectPlan::Submit(plan) = state.select(1, 1, 1, 1).unwrap() else {
                    panic!("distinct select")
                };
                assert!(gpu.select(plan.slot, plan.present_seq).unwrap());
                state.commit_submitted(plan).unwrap();
                assert_eq!(state.select(1, 1, 1, 2).unwrap(), SelectPlan::Alias);
                let replacement = filled_snapshot(&device, &queue, &[0.99; 35], stride, 5);
                assert_eq!(state.materialize(5).unwrap(), slot);
                gpu.materialize(&replacement, slot).unwrap();
                let SelectPlan::Submit(plan) = state.select(1, 1, 5, 5).unwrap() else {
                    panic!("second distinct select")
                };
                assert!(gpu.select(plan.slot, plan.present_seq).unwrap());
                state.commit_submitted(plan).unwrap();
                state.retire_through(5).unwrap();
                state.seal().unwrap();
                let mut frames = Vec::new();
                wait_until(|| {
                    frames.extend(gpu.drain_ready().unwrap());
                    frames.len() == 2
                });
                assert_eq!(
                    frames.iter().map(|f| f.present_seq).collect::<Vec<_>>(),
                    [1, 5]
                );
                let expected = depth_lab::decode::pool_nearest(&words, 7, 5, 3, 2, conv);
                let actual = frames.remove(0).decode().unwrap();
                assert_eq!(
                    actual.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
                    expected.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
                );
                assert!(
                    frames[0]
                        .decode()
                        .unwrap()
                        .iter()
                        .all(|value| *value == 0.99)
                );
                assert_eq!(gpu.completed(), state.stats().distinct_selected);
                assert_eq!(gpu.completed(), 2);
                assert_eq!(state.stats().source_identity_aliases, 1);
                wait_until(|| gpu.in_flight() == 0);
                gpu.refresh_final_stats().unwrap();
            }
        }
    }

    #[test]
    fn warp_app_depth_snapshot_is_gpu_only_and_leased_through_compute() {
        use crate::d3d12_abi::{DescriptorHandle, DsvIdentity, ResourceId};
        use crate::d3d12_capture::{D3d12CaptureRing, RecordResult};
        use windows::Win32::Graphics::Dxgi::Common::*;
        for (resource_format, dsv_format) in [
            (DXGI_FORMAT_D32_FLOAT, DXGI_FORMAT_D32_FLOAT),
            (DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT),
        ] {
            let (device, queue) = warp_device();
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: 7,
                Height: 5,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: resource_format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                ..Default::default()
            };
            let heap = D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                CreationNodeMask: 1,
                VisibleNodeMask: 1,
                ..Default::default()
            };
            let mut source = None;
            unsafe {
                device.CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    None,
                    &mut source,
                )
            }
            .unwrap();
            let source: ID3D12Resource = source.unwrap();
            let heap: ID3D12DescriptorHeap = unsafe {
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                    NumDescriptors: 1,
                    ..Default::default()
                })
            }
            .unwrap();
            let handle = unsafe { heap.GetCPUDescriptorHandleForHeapStart() };
            let view = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: dsv_format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_DSV { MipSlice: 0 },
                },
                ..Default::default()
            };
            unsafe { device.CreateDepthStencilView(&source, Some(&view), handle) };
            let identity = DsvIdentity {
                resource: ResourceId(1),
                handle: DescriptorHandle(handle.ptr as u64),
                descriptor_generation: 1,
                resource_epoch: 1,
            };
            let mut capture =
                D3d12CaptureRing::new_with_storage(&device, &source, identity, dsv_format, 4, true)
                    .unwrap();
            let mut gpu = D3d12CandidateRing::new(
                &device,
                &queue,
                &test_bootstrap(),
                4,
                capture.gpu_storage_bytes(),
                ZConvention::Traditional,
            )
            .unwrap();
            wait_until(|| gpu.ready().unwrap());
            let commands = create_commands(&device).unwrap();
            unsafe {
                commands
                    .list
                    .Reset(&commands.allocator, None::<&ID3D12PipelineState>)
                    .unwrap();
                commands.list.ClearDepthStencilView(
                    handle,
                    if dsv_format == DXGI_FORMAT_D24_UNORM_S8_UINT {
                        D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL
                    } else {
                        D3D12_CLEAR_FLAG_DEPTH
                    },
                    0.25,
                    173,
                    None,
                );
            }
            capture.begin_present_interval(1, true);
            let recorded = unsafe {
                capture.record_before_depth_write_exit(
                    commands.list.as_raw(),
                    &commands.list,
                    commands.list.vtable().ResourceBarrier as usize,
                    source.as_raw(),
                    Some(7),
                )
            }
            .unwrap();
            assert!(matches!(recorded, RecordResult::Recorded { .. }));
            unsafe {
                // Later reuse of the application's depth must not change Present 1's snapshot.
                commands
                    .list
                    .ClearDepthStencilView(handle, D3D12_CLEAR_FLAG_DEPTH, 0.75, 0, None);
                commands.list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(commands.list.cast().unwrap())]);
            }
            assert_eq!(
                capture
                    .on_execute_complete(&queue, &[commands.list.as_raw()])
                    .unwrap(),
                [(1, commands.list.as_raw() as usize, Some(7))]
            );
            assert!(
                capture
                    .drain_ready()
                    .unwrap_err()
                    .contains("must not be CPU-mapped")
            );
            let mut snapshots = capture.take_gpu_snapshots().unwrap();
            let snapshot = snapshots.pop().unwrap();
            assert!(snapshots.is_empty());
            assert!(capture.take_gpu_snapshots().unwrap().is_empty());
            let completion = gpu.materialize(&snapshot, 1).unwrap();
            capture
                .release_gpu_snapshot(&snapshot, Some(completion))
                .unwrap();
            assert!(capture.release_gpu_snapshot(&snapshot, None).is_err());
            assert!(gpu.select(1, 1).unwrap());
            let mut frames = Vec::new();
            wait_until(|| {
                frames.extend(gpu.drain_ready().unwrap());
                !frames.is_empty()
            });
            assert!(
                frames[0]
                    .decode()
                    .unwrap()
                    .iter()
                    .all(|value| (*value - 0.25).abs() <= 1.0 / 16_777_215.0)
            );
            wait_until(|| {
                assert!(capture.take_gpu_snapshots().unwrap().is_empty());
                capture.is_idle()
            });
            assert_eq!(capture.dropped_pool_full(), 0);
            assert_eq!(gpu.in_flight(), 0);
        }
    }
    #[test]
    fn allocation_includes_scratch_staging_alignment_and_rejects_overflow() {
        assert_eq!(allocation_bytes(4, 96, 8, 100).unwrap(), 105 * 65536 + 100);
        assert!(allocation_bytes(u64::MAX, 96, 8, 0).is_err());
        assert!(allocation_bytes(4, 0, 8, 0).is_err());
        assert!(allocation_bytes(4, 96, 0, 0).is_err());
        assert!(validate_budget(100, 99, 0, 1000).is_err());
        assert!(validate_budget(100, 100, u64::MAX, 1000).is_err());
        assert!(validate_budget(100, 100, 20, 119).is_err());
        assert!(validate_budget(100, 100, 20, 120).is_ok());
    }
    #[test]
    fn d32_and_d32s8_use_device_plane_stride() {
        assert!(validate_geometry(1920, 1080, 1280, 720, 7680, 4).is_ok());
        assert!(validate_geometry(1920, 1080, 1280, 720, 15360, 8).is_ok());
        assert!(validate_geometry(1920, 1080, 1280, 720, 7680, 8).is_err());
        assert!(validate_geometry(0, 1080, 1280, 720, 0, 4).is_err());
    }
}

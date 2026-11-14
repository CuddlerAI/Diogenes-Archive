//! Privately owned native descriptor heaps. Handles remain opaque, never dereferenced.
use super::device::Upload;
use windows::Win32::Graphics::Direct3D12::*;

pub(super) struct NativeDescriptors {
    device: ID3D12Device,
    // These owners outlive every scoped worker and all uses of their CPU handles.
    _source: ID3D12DescriptorHeap,
    _destination: ID3D12DescriptorHeap,
    source_start: usize,
    destination_start: usize,
    threads: usize,
    per_copy: usize,
    pub increment: usize,
    pub worker_stride_descriptors: usize,
    gpuva_start: u64,
}

impl NativeDescriptors {
    pub fn new(
        upload: &Upload,
        threads: usize,
        per_copy: usize,
        compact: bool,
    ) -> Result<Self, String> {
        // Query the device, never hardcode descriptor handle increments.
        let increment = unsafe {
            upload
                .device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
        } as usize;
        if increment == 0 || threads * per_copy * 2 * 256 > upload.owner.bytes as usize {
            return Err("native descriptor fixture bounds rejected".into());
        }
        // 4096 is only our hash's handle-integer grouping, NOT physical descriptor memory.
        let stride = if compact {
            per_copy
        } else {
            4096usize.div_ceil(increment).max(per_copy)
        };
        let create = |count, flags| -> Result<ID3D12DescriptorHeap, String> {
            unsafe {
                upload
                    .device
                    .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        NumDescriptors: count,
                        Flags: flags,
                        NodeMask: 0,
                    })
            }
            .map_err(|e| e.to_string())
        };
        // D3D12 descriptor-copy sources must be non-shader-visible.
        let source = create(
            (threads * stride * 2) as u32,
            D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        )?;
        let destination = create(
            (threads * stride) as u32,
            D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
        )?;
        let source_start = unsafe { source.GetCPUDescriptorHandleForHeapStart() }.ptr;
        let destination_start = unsafe { destination.GetCPUDescriptorHandleForHeapStart() }.ptr;
        if source_start == 0
            || destination_start == 0
            || source_start
                .checked_add(threads * stride * 2 * increment)
                .is_none()
            || destination_start
                .checked_add(threads * stride * increment)
                .is_none()
        {
            return Err("native descriptor handle extent rejected".into());
        }
        let fixture = Self {
            device: upload.device.clone(),
            _source: source,
            _destination: destination,
            source_start,
            destination_start,
            threads,
            per_copy,
            increment,
            worker_stride_descriptors: stride,
            gpuva_start: upload.gpuva(),
        };
        if fixture.gpuva_start == 0 || !fixture.gpuva_start.is_multiple_of(256) {
            return Err("native CBV resource address rejected".into());
        }
        for thread in 0..threads {
            for bank in 0..2 {
                for index in 0..per_copy {
                    unsafe {
                        fixture.device.CreateConstantBufferView(
                            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                                BufferLocation: fixture.gpuva(thread, bank, index),
                                SizeInBytes: 256,
                            }),
                            D3D12_CPU_DESCRIPTOR_HANDLE {
                                ptr: fixture.handle(thread, bank + 1, index) as usize,
                            },
                        );
                    }
                }
            }
        }
        Ok(fixture)
    }

    // bank 0 is the destination, banks 1/2 are immutable sources.
    pub fn handle(&self, thread: usize, bank: usize, index: usize) -> u64 {
        assert!(thread < self.threads && bank <= 2 && index < self.per_copy);
        let slot = thread * self.worker_stride_descriptors + index;
        let handle = if bank == 0 {
            self.destination_start + slot * self.increment
        } else {
            self.source_start
                + ((bank - 1) * self.threads * self.worker_stride_descriptors + slot)
                    * self.increment
        };
        handle as u64
    }

    pub fn gpuva(&self, thread: usize, bank: usize, index: usize) -> u64 {
        self.gpuva_start + ((thread * self.per_copy + index) * 2 + bank) as u64 * 256
    }

    pub fn copy(&self, thread: usize, bank: usize) {
        // Workers write disjoint destination ranges; sources never change. No GPU uses the heaps.
        unsafe {
            self.device.CopyDescriptorsSimple(
                self.per_copy as u32,
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: self.handle(thread, 0, 0) as usize,
                },
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: self.handle(thread, bank + 1, 0) as usize,
                },
                D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            );
        }
    }

    pub fn check_device(&self) -> Result<(), String> {
        unsafe { self.device.GetDeviceRemovedReason() }.map_err(|e| e.to_string())
    }
}

use super::*;
use windows::Win32::Graphics::{
    Direct3D::D3D_FEATURE_LEVEL_12_0,
    Direct3D12::*,
    Dxgi::{Common::DXGI_SAMPLE_DESC, *},
};
use windows::core::Interface;

pub(super) struct Upload {
    pub(super) device: ID3D12Device,
    resource: ID3D12Resource,
    pub owner: ObserverMappedIdentity,
    pub adapter: String,
    pub vendor_id: u32,
    pub device_id: u32,
}

impl Upload {
    pub(super) fn gpuva(&self) -> u64 {
        unsafe { self.resource.GetGPUVirtualAddress() }
    }

    pub fn new(bytes: usize, warp: bool) -> Result<Self, String> {
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.map_err(err)?;
        let adapter: IDXGIAdapter1 = if warp {
            unsafe { factory.EnumWarpAdapter() }.map_err(err)?
        } else {
            let mut selected = None;
            for index in 0..32 {
                let candidate = match unsafe { factory.EnumAdapters1(index) } {
                    Ok(value) => value,
                    Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(e) => return Err(err(e)),
                };
                let desc = unsafe { candidate.GetDesc1() }.map_err(err)?;
                if desc.VendorId == 0x10de && desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0
                {
                    selected = Some(candidate);
                    break;
                }
            }
            selected.ok_or("No NVIDIA hardware adapter; no silent WARP/iGPU fallback")?
        };
        let desc = unsafe { adapter.GetDesc1() }.map_err(err)?;
        let mut device = None;
        unsafe {
            D3D12CreateDevice::<_, ID3D12Device>(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)
        }
        .map_err(err)?;
        let device = device.ok_or("D3D12CreateDevice returned no device")?;
        let mut resource: Option<ID3D12Resource> = None;
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_UPLOAD,
                    CreationNodeMask: 1,
                    VisibleNodeMask: 1,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: bytes as u64,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut resource,
            )
        }
        .map_err(err)?;
        let resource = resource.ok_or("CreateCommittedResource returned no resource")?;
        let mut address = std::ptr::null_mut();
        // We really do read the upload mapping. Do not claim a write-only Map range.
        unsafe { resource.Map(0, None, Some(&mut address)) }.map_err(err)?;
        if address.is_null() {
            unsafe { resource.Unmap(0, None) };
            return Err("Map returned null".into());
        }
        // Initialise every byte before workers are published. No CPU/GPU writer exists later.
        unsafe { std::ptr::write_bytes(address, 0x5a, bytes) };
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_sfence()
        };
        Ok(Self {
            device,
            owner: ObserverMappedIdentity {
                resource_raw: resource.as_raw() as usize,
                source_tag: 1,
                content_generation: 0,
                base: address as usize,
                bytes: bytes as u64,
            },
            resource,
            adapter: String::from_utf16_lossy(
                &desc.Description[..desc
                    .Description
                    .iter()
                    .position(|&v| v == 0)
                    .unwrap_or(desc.Description.len())],
            ),
            vendor_id: desc.VendorId,
            device_id: desc.DeviceId,
        })
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        // All scoped workers have joined before this owner can be dropped, including errors.
        unsafe { self.resource.Unmap(0, None) };
    }
}

fn err(error: windows::core::Error) -> String {
    error.to_string()
}

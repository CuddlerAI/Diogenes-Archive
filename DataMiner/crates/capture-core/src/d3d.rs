use anyhow::{Context, anyhow, ensure};
use serde::Serialize;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1,
    IDXGIDevice, IDXGIFactory1,
};
use windows::core::Interface;

pub const NVIDIA_VENDOR_ID: u32 = 0x10DE;

// Optimus reads this DWORD from the executable's PE exports before startup.
// Explicitly creating our D3D11 device on NVIDIA does not opt the host process
// in: without this export, NVIDIA's MFT can fail activation on hybrid laptops.
// Executable build scripts must also call configure_nvidia_optimus_exports().
// This is a selection hint; adapter and hardware-only preflight remain required.
#[cfg(all(target_os = "windows", target_env = "msvc"))]
#[used]
#[unsafe(export_name = "NvOptimusEnablement")]
pub static NVIDIA_OPTIMUS_ENABLEMENT: u32 = 1;

#[derive(Clone, Debug, Serialize)]
pub struct AdapterIdentity {
    pub description: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub luid_high: i32,
    pub luid_low: u32,
    pub feature_level: u32,
    pub driver_version: String,
}

pub struct D3dContext {
    pub device: ID3D11Device,
    pub immediate_context: ID3D11DeviceContext,
    pub adapter: AdapterIdentity,
}

impl D3dContext {
    pub fn create_nvidia_hardware_only() -> anyhow::Result<Self> {
        let factory: IDXGIFactory1 =
            unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1 failed")?;
        let (adapter, desc) = find_nvidia_adapter(&factory)?;
        let driver_version = adapter_driver_version(&adapter)?;

        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device = None;
        let mut context = None;
        let mut selected_level = D3D_FEATURE_LEVEL::default();
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut selected_level),
                Some(&mut context),
            )
        }
        .context("D3D11CreateDevice on the NVIDIA adapter failed")?;
        let device = device.context("D3D11 returned no device")?;
        let immediate_context = context.context("D3D11 returned no immediate context")?;
        ensure!(
            selected_level.0 >= D3D_FEATURE_LEVEL_11_0.0,
            "D3D feature level 11.0 is required"
        );

        let multithread: ID3D11Multithread = immediate_context
            .cast()
            .context("D3D11 multithread protection is unavailable")?;
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        Ok(Self {
            device,
            immediate_context,
            adapter: AdapterIdentity {
                description: utf16_z(&desc.Description),
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                luid_high: desc.AdapterLuid.HighPart,
                luid_low: desc.AdapterLuid.LowPart,
                feature_level: selected_level.0 as u32,
                driver_version,
            },
        })
    }
}

pub(crate) fn adapter_driver_version(adapter: &IDXGIAdapter1) -> anyhow::Result<String> {
    let raw_version = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
        .context("IDXGIAdapter::CheckInterfaceSupport(IDXGIDevice) failed")?;
    Ok(format_driver_version(raw_version))
}

fn format_driver_version(raw: i64) -> String {
    let bits = raw as u64;
    let high = (bits >> 32) as u32;
    let low = bits as u32;
    format!(
        "{}.{}.{}.{}",
        high >> 16,
        high & 0xffff,
        low >> 16,
        low & 0xffff
    )
}

fn find_nvidia_adapter(
    factory: &IDXGIFactory1,
) -> anyhow::Result<(
    IDXGIAdapter1,
    windows::Win32::Graphics::Dxgi::DXGI_ADAPTER_DESC1,
)> {
    let mut index = 0;
    loop {
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error).context("DXGI adapter enumeration failed"),
        };
        let desc = unsafe { adapter.GetDesc1() }.context("IDXGIAdapter1::GetDesc1 failed")?;
        if desc.VendorId == NVIDIA_VENDOR_ID
            && desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0
        {
            return Ok((adapter, desc));
        }
        index += 1;
    }
    Err(anyhow!(
        "no NVIDIA hardware adapter was found; P0 forbids adapter fallback"
    ))
}

fn utf16_z(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dxgi_driver_version_as_four_components() {
        let high = 32_u64 << 16;
        let low = (15_u64 << 16) | 8253;
        assert_eq!(
            format_driver_version(((high << 32) | low) as i64),
            "32.0.15.8253"
        );
    }
}

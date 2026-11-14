use anyhow::{Context, anyhow, ensure};
use serde::Serialize;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE, DXGI_ERROR_NOT_FOUND, IDXGIAdapter1,
    IDXGIFactory1,
};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

use crate::d3d::{NVIDIA_VENDOR_ID, adapter_driver_version};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuSnapshot {
    pub pci_id: String,
    pub driver_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplaySnapshot {
    pub schema_version: &'static str,
    pub monitors: Vec<DisplayMonitorSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DisplayMonitorSnapshot {
    pub width_px: u32,
    pub height_px: u32,
    pub primary: bool,
}

pub fn collect_nvidia_gpu_snapshot() -> anyhow::Result<GpuSnapshot> {
    let factory = create_factory()?;
    let (adapter, desc) = find_nvidia_adapter(&factory)?;

    Ok(GpuSnapshot {
        pci_id: format!("PCI\\VEN_{:04X}&DEV_{:04X}", desc.VendorId, desc.DeviceId),
        driver_version: adapter_driver_version(&adapter)?,
    })
}

pub fn collect_display_snapshot() -> anyhow::Result<DisplaySnapshot> {
    let factory = create_factory()?;
    let mut monitors = Vec::new();
    let mut adapter_index = 0;

    loop {
        let adapter = match unsafe { factory.EnumAdapters1(adapter_index) } {
            Ok(adapter) => adapter,
            Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(error) => return Err(error).context("DXGI adapter enumeration failed"),
        };
        let mut output_index = 0;
        loop {
            let output = match unsafe { adapter.EnumOutputs(output_index) } {
                Ok(output) => output,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error).context("DXGI output enumeration failed"),
            };
            let desc = unsafe { output.GetDesc() }.context("IDXGIOutput::GetDesc failed")?;
            if desc.AttachedToDesktop.as_bool() {
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                ensure!(
                    unsafe { GetMonitorInfoW(desc.Monitor, &mut info) }.as_bool(),
                    "GetMonitorInfoW failed"
                );
                let (width_px, height_px) = monitor_geometry(info.rcMonitor)?;
                monitors.push(DisplayMonitorSnapshot {
                    width_px,
                    height_px,
                    primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
                });
            }
            output_index += 1;
        }
        adapter_index += 1;
    }

    monitors.sort_by_key(|monitor| (!monitor.primary, monitor.width_px, monitor.height_px));
    Ok(DisplaySnapshot {
        schema_version: "dm-display-snapshot-1",
        monitors,
    })
}

fn create_factory() -> anyhow::Result<IDXGIFactory1> {
    unsafe { CreateDXGIFactory1() }.context("CreateDXGIFactory1 failed")
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
    Err(anyhow!("no NVIDIA hardware adapter was found"))
}

fn monitor_geometry(rect: windows::Win32::Foundation::RECT) -> anyhow::Result<(u32, u32)> {
    let width = rect
        .right
        .checked_sub(rect.left)
        .context("monitor width overflow")?;
    let height = rect
        .bottom
        .checked_sub(rect.top)
        .context("monitor height overflow")?;
    ensure!(width > 0 && height > 0, "monitor geometry is empty");
    Ok((width as u32, height as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::RECT;

    #[test]
    fn monitor_geometry_accepts_negative_virtual_desktop_coordinates() {
        assert_eq!(
            monitor_geometry(RECT {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1080,
            })
            .unwrap(),
            (1920, 1080)
        );
    }

    #[test]
    fn monitor_geometry_rejects_empty_rectangles() {
        assert!(
            monitor_geometry(RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 1080,
            })
            .is_err()
        );
    }

    #[test]
    #[ignore = "requires an interactive Windows machine with NVIDIA hardware"]
    fn current_hardware_snapshot_smoke() {
        let gpu = collect_nvidia_gpu_snapshot().unwrap();
        let displays = collect_display_snapshot().unwrap();
        eprintln!("gpu={gpu:?} displays={displays:?}");
        assert!(gpu.pci_id.starts_with("PCI\\VEN_10DE&DEV_"));
        assert_eq!(gpu.driver_version.split('.').count(), 4);
        assert!(!displays.monitors.is_empty());
    }
}

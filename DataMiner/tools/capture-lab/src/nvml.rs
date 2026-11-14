use std::ffi::{CStr, c_char, c_void};

use serde::Serialize;
use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{PCSTR, w};

type NvmlDevice = *mut c_void;
type InitFn = unsafe extern "C" fn() -> i32;
type ShutdownFn = unsafe extern "C" fn() -> i32;
type DeviceByIndexFn = unsafe extern "C" fn(u32, *mut NvmlDevice) -> i32;
type UtilizationFn = unsafe extern "C" fn(NvmlDevice, *mut NvmlUtilization) -> i32;
type EncoderUtilizationFn = unsafe extern "C" fn(NvmlDevice, *mut u32, *mut u32) -> i32;
type DeviceNameFn = unsafe extern "C" fn(NvmlDevice, *mut c_char, u32) -> i32;

const NVML_SUCCESS: i32 = 0;

#[repr(C)]
#[derive(Default)]
struct NvmlUtilization {
    gpu: u32,
    memory: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct GpuReport {
    pub available: bool,
    pub device_name: String,
    pub samples: usize,
    pub gpu_average_percent: f64,
    pub gpu_max_percent: u32,
    pub encoder_average_percent: f64,
    pub encoder_max_percent: u32,
}

pub struct NvmlSampler {
    module: HMODULE,
    shutdown: ShutdownFn,
    utilization: UtilizationFn,
    encoder_utilization: EncoderUtilizationFn,
    device: NvmlDevice,
    device_name: String,
    samples: Vec<(u32, u32)>,
}

impl NvmlSampler {
    pub fn load() -> anyhow::Result<Self> {
        let module = unsafe { LoadLibraryW(w!("nvml.dll")) }
            .map_err(|error| anyhow::anyhow!("nvml.dll unavailable: {error}"))?;
        let init: InitFn = unsafe { std::mem::transmute(required(module, b"nvmlInit_v2\0")?) };
        let shutdown: ShutdownFn =
            unsafe { std::mem::transmute(required(module, b"nvmlShutdown\0")?) };
        let device_by_index: DeviceByIndexFn =
            unsafe { std::mem::transmute(required(module, b"nvmlDeviceGetHandleByIndex_v2\0")?) };
        let utilization: UtilizationFn =
            unsafe { std::mem::transmute(required(module, b"nvmlDeviceGetUtilizationRates\0")?) };
        let encoder_utilization: EncoderUtilizationFn =
            unsafe { std::mem::transmute(required(module, b"nvmlDeviceGetEncoderUtilization\0")?) };
        let device_name_fn: DeviceNameFn =
            unsafe { std::mem::transmute(required(module, b"nvmlDeviceGetName\0")?) };

        ensure_nvml(unsafe { init() }, "nvmlInit_v2")?;
        let mut device = std::ptr::null_mut();
        ensure_nvml(
            unsafe { device_by_index(0, &mut device) },
            "nvmlDeviceGetHandleByIndex_v2",
        )?;
        let mut name = [0_i8; 128];
        ensure_nvml(
            unsafe { device_name_fn(device, name.as_mut_ptr(), name.len() as u32) },
            "nvmlDeviceGetName",
        )?;
        let device_name = unsafe { CStr::from_ptr(name.as_ptr()) }
            .to_string_lossy()
            .into_owned();

        Ok(Self {
            module,
            shutdown,
            utilization,
            encoder_utilization,
            device,
            device_name,
            samples: Vec::new(),
        })
    }

    #[must_use]
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    pub fn sample(&mut self) -> anyhow::Result<()> {
        let mut utilization = NvmlUtilization::default();
        ensure_nvml(
            unsafe { (self.utilization)(self.device, &mut utilization) },
            "nvmlDeviceGetUtilizationRates",
        )?;
        let mut encoder = 0_u32;
        let mut sampling_period_us = 0_u32;
        ensure_nvml(
            unsafe {
                (self.encoder_utilization)(self.device, &mut encoder, &mut sampling_period_us)
            },
            "nvmlDeviceGetEncoderUtilization",
        )?;
        self.samples.push((utilization.gpu, encoder));
        Ok(())
    }

    pub fn report(&self) -> GpuReport {
        if self.samples.is_empty() {
            return GpuReport {
                available: true,
                device_name: self.device_name.clone(),
                ..Default::default()
            };
        }
        GpuReport {
            available: true,
            device_name: self.device_name.clone(),
            samples: self.samples.len(),
            gpu_average_percent: self
                .samples
                .iter()
                .map(|sample| sample.0 as f64)
                .sum::<f64>()
                / self.samples.len() as f64,
            gpu_max_percent: self
                .samples
                .iter()
                .map(|sample| sample.0)
                .max()
                .unwrap_or(0),
            encoder_average_percent: self
                .samples
                .iter()
                .map(|sample| sample.1 as f64)
                .sum::<f64>()
                / self.samples.len() as f64,
            encoder_max_percent: self
                .samples
                .iter()
                .map(|sample| sample.1)
                .max()
                .unwrap_or(0),
        }
    }
}

impl Drop for NvmlSampler {
    fn drop(&mut self) {
        let _ = unsafe { (self.shutdown)() };
        let _ = unsafe { FreeLibrary(self.module) };
    }
}

fn required(
    module: HMODULE,
    name: &'static [u8],
) -> anyhow::Result<unsafe extern "system" fn() -> isize> {
    let function = unsafe { GetProcAddress(module, PCSTR(name.as_ptr())) };
    function.ok_or_else(|| {
        anyhow::anyhow!(
            "NVML entry point {} is unavailable",
            String::from_utf8_lossy(&name[..name.len() - 1])
        )
    })
}

fn ensure_nvml(result: i32, operation: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        result == NVML_SUCCESS,
        "{operation} failed with NVML status {result}"
    );
    Ok(())
}

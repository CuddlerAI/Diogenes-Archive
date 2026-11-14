//! Read-only inventory; does not activate encoders or capture anything.
use capture_core::mf::MfPlatform;
use serde_json::json;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_HARDWARE_URL_Attribute, MFT_ENUM_HARDWARE_VENDOR_ID_Attribute,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFT_TRANSFORM_CLSID_Attribute, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{
    COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::core::GUID;

struct ComGuard;
impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct Activations {
    pointer: *mut Option<IMFActivate>,
    count: u32,
}
impl Drop for Activations {
    fn drop(&mut self) {
        if !self.pointer.is_null() {
            unsafe {
                for index in 0..self.count as usize {
                    std::ptr::drop_in_place(self.pointer.add(index));
                }
                CoTaskMemFree(Some(self.pointer.cast()));
            }
        }
    }
}

fn string(activation: &IMFActivate, key: &GUID) -> Option<String> {
    let length = unsafe { activation.GetStringLength(key) }.ok()?;
    let mut buffer = vec![0; length as usize + 1];
    unsafe { activation.GetString(key, &mut buffer, None) }.ok()?;
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

fn main() -> anyhow::Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok()?;
    let _com = ComGuard;
    let _mf = MfPlatform::startup()?;
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activations = Activations {
        pointer: std::ptr::null_mut(),
        count: 0,
    };
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE,
            Some(&raw const input),
            Some(&raw const output),
            &raw mut activations.pointer,
            &raw mut activations.count,
        )?
    };
    anyhow::ensure!(
        activations.count == 0 || !activations.pointer.is_null(),
        "null activation array"
    );
    let mut records = Vec::new();
    for index in 0..activations.count as usize {
        let activation = unsafe { &*activations.pointer.add(index) }
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("null activation entry"))?;
        records.push(json!({
            "friendly_name": string(activation, &MFT_FRIENDLY_NAME_Attribute),
            "hardware_url": string(activation, &MFT_ENUM_HARDWARE_URL_Attribute),
            "vendor_id": string(activation, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
            "clsid": unsafe { activation.GetGUID(&MFT_TRANSFORM_CLSID_Attribute) }.ok().map(|g| format!("{g:?}")),
        }));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"hardware_encoders":records}))?
    );
    Ok(())
}

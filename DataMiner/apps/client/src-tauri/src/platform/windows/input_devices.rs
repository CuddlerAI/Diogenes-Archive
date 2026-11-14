use anyhow::{Context, ensure};
use serde::Serialize;
use windows::Win32::UI::Input::{
    GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RID_DEVICE_INFO,
    RID_DEVICE_INFO_TYPE, RIDI_DEVICEINFO, RIM_TYPEHID, RIM_TYPEKEYBOARD, RIM_TYPEMOUSE,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputDevicesSnapshot {
    pub schema_version: &'static str,
    pub keyboards: u32,
    pub mice: u32,
    pub hids: u32,
}

pub fn collect_input_devices_snapshot() -> anyhow::Result<InputDevicesSnapshot> {
    let list_size = u32::try_from(std::mem::size_of::<RAWINPUTDEVICELIST>())
        .context("RAWINPUTDEVICELIST size exceeds u32")?;
    let mut count = 0_u32;
    let queried = unsafe { GetRawInputDeviceList(None, &mut count, list_size) };
    ensure!(queried != u32::MAX, "GetRawInputDeviceList(size) failed");

    let capacity = usize::try_from(count).context("Raw Input device count exceeds usize")?;
    let mut devices = vec![RAWINPUTDEVICELIST::default(); capacity];
    if count > 0 {
        let read =
            unsafe { GetRawInputDeviceList(Some(devices.as_mut_ptr()), &mut count, list_size) };
        ensure!(read != u32::MAX, "GetRawInputDeviceList(payload) failed");
        ensure!(read == count, "Raw Input device list changed while reading");
        devices.truncate(usize::try_from(read).context("Raw Input result count exceeds usize")?);
    }

    let mut snapshot = InputDevicesSnapshot {
        schema_version: "dm-input-devices-1",
        keyboards: 0,
        mice: 0,
        hids: 0,
    };
    let info_size = u32::try_from(std::mem::size_of::<RID_DEVICE_INFO>())
        .context("RID_DEVICE_INFO size exceeds u32")?;
    for device in devices {
        // Windows input stacks may expose undocumented Raw Input device types. They are
        // outside this aggregate keyboard/mouse/HID contract, so ignore them while
        // retaining strict detail validation for every recognized device type.
        if !is_known_input_device_type(device.dwType) {
            continue;
        }
        let mut info = RID_DEVICE_INFO {
            cbSize: info_size,
            ..Default::default()
        };
        let mut payload_size = info_size;
        let read = unsafe {
            GetRawInputDeviceInfoW(
                Some(device.hDevice),
                RIDI_DEVICEINFO,
                Some((&mut info as *mut RID_DEVICE_INFO).cast()),
                &mut payload_size,
            )
        };
        ensure!(read != u32::MAX, "GetRawInputDeviceInfoW failed");
        ensure!(
            read == info_size && payload_size == info_size,
            "GetRawInputDeviceInfoW returned an unexpected payload size"
        );
        ensure!(
            info.dwType == device.dwType,
            "Raw Input device type changed while reading"
        );
        if info.dwType == RIM_TYPEKEYBOARD {
            snapshot.keyboards = snapshot
                .keyboards
                .checked_add(1)
                .context("Raw Input keyboard count overflow")?;
        } else if info.dwType == RIM_TYPEMOUSE {
            snapshot.mice = snapshot
                .mice
                .checked_add(1)
                .context("Raw Input mouse count overflow")?;
        } else if info.dwType == RIM_TYPEHID {
            snapshot.hids = snapshot
                .hids
                .checked_add(1)
                .context("Raw Input HID count overflow")?;
        }
    }
    Ok(snapshot)
}

fn is_known_input_device_type(device_type: RID_DEVICE_INFO_TYPE) -> bool {
    device_type == RIM_TYPEKEYBOARD || device_type == RIM_TYPEMOUSE || device_type == RIM_TYPEHID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_snapshot_contains_only_aggregate_counts() {
        let value = serde_json::to_value(InputDevicesSnapshot {
            schema_version: "dm-input-devices-1",
            keyboards: 1,
            mice: 2,
            hids: 3,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "schemaVersion": "dm-input-devices-1",
                "keyboards": 1,
                "mice": 2,
                "hids": 3,
            })
        );
    }

    #[test]
    fn recognizes_only_the_input_types_in_the_snapshot_contract() {
        assert!(is_known_input_device_type(RIM_TYPEKEYBOARD));
        assert!(is_known_input_device_type(RIM_TYPEMOUSE));
        assert!(is_known_input_device_type(RIM_TYPEHID));
        assert!(!is_known_input_device_type(RID_DEVICE_INFO_TYPE(3)));
    }

    #[test]
    #[ignore = "requires an interactive Windows machine"]
    fn current_input_device_snapshot_smoke() {
        let snapshot = collect_input_devices_snapshot().unwrap();
        eprintln!("input_devices={snapshot:?}");
        assert!(snapshot.keyboards + snapshot.mice + snapshot.hids > 0);
    }
}

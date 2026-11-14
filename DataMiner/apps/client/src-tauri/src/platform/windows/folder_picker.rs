use std::ffi::c_void;
use std::path::PathBuf;

use anyhow::Context;
use tauri::WebviewWindow;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::{
    FOS_DONTADDTORECENT, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
    IFileOpenDialog, SIGDN_FILESYSPATH,
};
use windows::core::HSTRING;

const HRESULT_CANCELLED: u32 = 0x8007_04C7;

struct ComApartment;

impl ComApartment {
    fn initialize() -> anyhow::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .context("failed to initialize the native folder picker")?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub fn choose_folder(window: &WebviewWindow) -> anyhow::Result<Option<PathBuf>> {
    let owner = HWND(
        window
            .hwnd()
            .context("main window handle is unavailable")?
            .0,
    );
    let _apartment = ComApartment::initialize()?;
    let dialog: IFileOpenDialog =
        unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) }
            .context("failed to create the native folder picker")?;
    let options =
        unsafe { dialog.GetOptions() }.context("failed to inspect native folder picker options")?;
    unsafe {
        dialog.SetOptions(
            options
                | FOS_PICKFOLDERS
                | FOS_FORCEFILESYSTEM
                | FOS_PATHMUSTEXIST
                | FOS_DONTADDTORECENT,
        )
    }
    .context("failed to configure the native folder picker")?;
    let title = HSTRING::from("选择采集数据位置（将创建 DiogenesDataMiner 子目录）");
    unsafe { dialog.SetTitle(&title) }.context("failed to label the native folder picker")?;
    if let Err(error) = unsafe { dialog.Show(Some(owner)) } {
        if error.code().0 as u32 == HRESULT_CANCELLED {
            return Ok(None);
        }
        return Err(error).context("native folder picker failed");
    }
    let item = unsafe { dialog.GetResult() }.context("folder picker returned no selection")?;
    let display_name = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
        .context("selected folder has no filesystem path")?;
    let path = unsafe { display_name.to_string() };
    unsafe { CoTaskMemFree(Some(display_name.0.cast::<c_void>())) };
    Ok(Some(PathBuf::from(
        path.context("selected folder path is not valid Unicode")?,
    )))
}

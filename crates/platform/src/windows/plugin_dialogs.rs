//! Native Windows dialogs and Explorer integration for Plugin management.
//! Lives in `glph-platform` so `crates/app`'s `i18n` wide-literal test stays
//! focused purely on user-facing display strings.
use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
};
use windows_sys::Win32::{
    Foundation::HWND,
    UI::{
        Controls::Dialogs::{
            GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
            OPENFILENAMEW,
        },
        Shell::ShellExecuteW,
        WindowsAndMessaging::SW_SHOWNORMAL,
    },
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Ensures `path` exists and opens it in Windows Explorer.
pub fn open_plugins_folder(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("create plugins folder: {e}"))?;
    let verb = wide("explore");
    let target: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: Null-terminated UTF-16 strings; ShellExecuteW returns > 32 on success.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result <= 32 {
        return Err(format!("ShellExecuteW failed with code {result}"));
    }
    Ok(())
}

/// Opens the native Windows file picker for `.glph`, `.zip`, or `plugin.json`
/// bundles. Runs synchronously on the calling worker thread.
pub fn pick_plugin_file(
    owner_hwnd: isize,
    title: &str,
    filter_label: &str,
) -> Result<Option<PathBuf>, String> {
    let mut filter: Vec<u16> = Vec::new();
    filter.extend(filter_label.encode_utf16());
    filter.push(0);
    filter.extend("*.glph;*.zip;plugin.json".encode_utf16());
    filter.push(0);
    filter.push(0);

    let title_w = wide(title);
    let mut file_buf = vec![0u16; 4096];
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner_hwnd as HWND;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = file_buf.as_mut_ptr();
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.lpstrTitle = title_w.as_ptr();
    ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR;

    // SAFETY: All buffers live for the duration of GetOpenFileNameW and are zero-terminated.
    let ok = unsafe { GetOpenFileNameW(&mut ofn) };
    if ok == 0 {
        return Ok(None);
    }
    let len = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    if len == 0 {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(OsString::from_wide(&file_buf[..len]))))
}

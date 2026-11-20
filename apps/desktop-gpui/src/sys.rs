//! Shell integration: open Explorer at a path, send a folder to the Recycle Bin.

use std::process::Command;
use windows_sys::Win32::UI::Shell::{
    SHFileOperationW, SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FOF_SILENT,
};

/// True when Windows is set to light mode for apps
/// (`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize` →
/// `AppsUseLightTheme`). Unreadable keys fall back to the dark default.
pub fn system_prefers_light() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD,
    };

    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect();
    let value: Vec<u16> = "AppsUseLightTheme\0".encode_utf16().collect();

    let mut data: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let mut kind: u32 = 0;
    // SAFETY: the two strings are NUL-terminated UTF-16 buffers that outlive the
    // call, and `data`/`size` describe a writable u32 for a DWORD query.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            &mut kind,
            &mut data as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    status == 0 && data != 0
}

/// Reveal `path` in Explorer: a folder opens itself, a file is selected inside
/// its parent. `is_dir` comes from the scan rather than the filesystem so a
/// folder that vanished since the scan still opens where the user expects.
pub fn reveal(path: &str, is_dir: bool) {
    let status = if is_dir {
        Command::new("explorer.exe").arg(path).spawn()
    } else {
        Command::new("explorer.exe")
            .arg(format!("/select,{path}"))
            .spawn()
    };
    if let Err(error) = status {
        eprintln!("could not open Explorer: {error}");
    }
}

/// Handle of the window that owns the current keyboard focus, used as the owner
/// of the confirmation box so it stays attached to this app.
pub fn active_window() -> isize {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    unsafe { GetActiveWindow() as isize }
}

static ICON_TARGET: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "system" fn find_own_window(
    hwnd: windows_sys::Win32::Foundation::HWND,
    _param: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::BOOL {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindowVisible};

    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, &mut pid);
    if pid == std::process::id() && IsWindowVisible(hwnd) != 0 {
        ICON_TARGET.store(hwnd as isize, std::sync::atomic::Ordering::SeqCst);
        return 0;
    }
    1
}

/// Give the top-level window the sunburst icon embedded by `build.rs`.
///
/// GPUI never sets `WM_SETICON`, so without this the taskbar and alt-tab fall
/// back to the generic application icon.
pub fn apply_window_icon() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
    };

    // SAFETY: the callback only touches a static atomic and the Win32 handles
    // are checked for null before use; resource id 1 is the icon in `app.rc`.
    unsafe {
        ICON_TARGET.store(0, std::sync::atomic::Ordering::SeqCst);
        EnumWindows(Some(find_own_window), 0);
        let hwnd = ICON_TARGET.load(std::sync::atomic::Ordering::SeqCst);
        if hwnd == 0 {
            return;
        }
        let hwnd = hwnd as windows_sys::Win32::Foundation::HWND;
        let module = GetModuleHandleW(std::ptr::null());
        let id = 1 as *const u16;

        let big = LoadImageW(
            module,
            id,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXICON),
            GetSystemMetrics(SM_CYICON),
            LR_DEFAULTCOLOR,
        );
        let small = LoadImageW(
            module,
            id,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_DEFAULTCOLOR,
        );
        if !big.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as isize);
        }
        if !small.is_null() {
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as isize);
        }
    }
}

/// Ask a yes/no question in a native Windows message box.
///
/// Runs on a background thread so the UI keeps painting while it is open; the
/// default button is **No**, and closing the box answers No.
pub fn confirm(owner: isize, title: &str, text: &str) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_DEFBUTTON2, MB_ICONWARNING, MB_YESNO, IDYES,
    };

    let to_wide = |value: &str| -> Vec<u16> {
        value
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let text = to_wide(text);
    let title = to_wide(title);

    // SAFETY: both strings are NUL-terminated UTF-16 and outlive the call; the
    // owner handle is a plain window handle owned by this process.

    let answer = unsafe {
        MessageBoxW(
            owner as *mut core::ffi::c_void,
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };

    answer == IDYES
}

/// Move `path` to the Recycle Bin. Returns a human readable error on failure.
pub fn recycle(path: &str) -> Result<(), String> {
    let target = std::path::Path::new(path);
    if target.parent().is_none() {
        return Err("refusing to delete a drive root".to_string());
    }
    if !target.exists() {
        return Err(format!("{} no longer exists", path));
    }

    // `pFrom` is a double-NUL terminated list of paths.
    let mut from: Vec<u16> = path.encode_utf16().collect();
    from.push(0);
    from.push(0);

    let mut operation = SHFILEOPSTRUCTW {
        hwnd: std::ptr::null_mut(),
        wFunc: FO_DELETE as u32,
        pFrom: from.as_ptr(),
        pTo: std::ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: std::ptr::null(),
    };

    let code = unsafe { SHFileOperationW(&mut operation) };
    if operation.fAnyOperationsAborted != 0 {
        return Err("delete was cancelled".to_string());
    }
    if code != 0 {
        return Err(format!("could not move to the Recycle Bin (error {code})"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycles_a_file_and_refuses_a_drive_root() {
        let dir = std::env::temp_dir().join(format!("disk-scanner-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("recycle-me.txt");
        std::fs::write(&file, b"bye").expect("write fixture");

        recycle(file.to_str().expect("utf8 path")).expect("recycle should succeed");
        assert!(!file.exists(), "file should be out of the working tree");

        let _ = std::fs::remove_dir(&dir);
        assert!(recycle("C:\\").is_err(), "a drive root must be rejected");
    }
}

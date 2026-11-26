//! Shell integration: reveal a path in the file manager, move items to the
//! trash, ask a yes/no question and follow the system appearance.
//!
//! Every entry point here has one implementation per platform in this file, so
//! the app calls the same names on Windows, macOS and Linux.

use std::process::Command;

#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{
    SHFileOperationW, SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FOF_SILENT,
};

/// True when Windows is set to light mode for apps
/// (`HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize` →
/// `AppsUseLightTheme`). Unreadable keys fall back to the dark default.
#[cfg(windows)]
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

/// The appearance is asked of the desktop through a helper process, and the app
/// re-checks it every few seconds, so an answer is reused for a moment instead
/// of spawning one every time.
#[cfg(unix)]
fn cached_probe(probe: impl FnOnce() -> bool) -> bool {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    static LAST: OnceLock<Mutex<Option<(Instant, bool)>>> = OnceLock::new();
    let cache = LAST.get_or_init(|| Mutex::new(None));
    let Ok(mut slot) = cache.lock() else {
        // A poisoned lock only means another thread panicked here; the answer
        // itself is still available by asking again.
        return probe();
    };
    if let Some((at, value)) = *slot {
        if at.elapsed() < Duration::from_secs(10) {
            return value;
        }
    }
    let value = probe();
    *slot = Some((Instant::now(), value));
    value
}

/// True in light mode: `AppleInterfaceStyle` exists only while dark mode is on,
/// so its absence is the light answer. A machine where the key cannot be read
/// keeps the light default a stock desktop starts in.
#[cfg(target_os = "macos")]
pub fn system_prefers_light() -> bool {
    cached_probe(|| {
        Command::new("defaults")
            .args(["read", "-g", "AppleInterfaceStyle"])
            .output()
            .map(|output| {
                let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
                !(output.status.success() && text.contains("dark"))
            })
            .unwrap_or(true)
    })
}

/// True in light mode: GNOME publishes the answer as a colour scheme, and a
/// theme whose name says "dark" settles it on desktops that do not.
#[cfg(all(unix, not(target_os = "macos")))]
pub fn system_prefers_light() -> bool {
    cached_probe(|| {
        let read = |key: &str| -> Option<String> {
            let output = Command::new("gsettings")
                .args(["get", "org.gnome.desktop.interface", key])
                .output()
                .ok()?;
            if !output.status.success() {
                return None;
            }
            Some(String::from_utf8_lossy(&output.stdout).to_lowercase())
        };

        if let Some(scheme) = read("color-scheme") {
            if scheme.contains("dark") {
                return false;
            }
            if scheme.contains("light") || scheme.contains("default") {
                return true;
            }
        }
        match read("gtk-theme") {
            Some(theme) => !theme.contains("dark"),
            None => true,
        }
    })
}

/// Reveal `path` in Explorer: a folder opens itself, a file is selected inside
/// its parent. `is_dir` comes from the scan rather than the filesystem so a
/// folder that vanished since the scan still opens where the user expects.
#[cfg(windows)]
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

/// Reveal `path` in the desktop file manager: macOS selects the item inside its
/// parent with `open -R`, Linux opens the folder with `xdg-open`.
#[cfg(unix)]
pub fn reveal(path: &str, is_dir: bool) {
    #[cfg(target_os = "macos")]
    let args: Vec<String> = if is_dir {
        vec![path.to_string()]
    } else {
        vec!["-R".to_string(), path.to_string()]
    };
    #[cfg(not(target_os = "macos"))]
    let args: Vec<String> = {
        let target = if is_dir {
            std::path::PathBuf::from(path)
        } else {
            std::path::Path::new(path)
                .parent()
                .map(|parent| parent.to_path_buf())
                .unwrap_or_else(|| std::path::PathBuf::from(path))
        };
        vec![target.to_string_lossy().into_owned()]
    };

    if let Err(error) = Command::new("xdg-open").args(&args).spawn() {
        eprintln!("could not open the file manager: {error}");
    }
}

/// Open `url` in the user's browser: the release page, when the update card
/// offers one.
#[cfg(windows)]
pub fn open_url(url: &str) {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let open: Vec<u16> = "open\0".encode_utf16().collect();
    let url: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: both strings are NUL-terminated UTF-16 buffers that outlive the
    // call; the returned handle is only checked against "did it open".
    let handle = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            open.as_ptr(),
            url.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    if handle as isize <= 32 {
        eprintln!("could not open the browser (ShellExecuteW returned {handle:?})");
    }
}

#[cfg(unix)]
pub fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(not(target_os = "macos"))]
    let program = "xdg-open";

    if let Err(error) = Command::new(program).arg(url).spawn() {
        eprintln!("could not open the browser: {error}");
    }
}

/// Handle of the window that owns the current keyboard focus, used as the owner
/// of the confirmation box so it stays attached to this app.
#[cfg(windows)]
pub fn active_window() -> isize {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

    unsafe { GetActiveWindow() as isize }
}

/// A shell dialog on Unix is its own process, so no owner handle is needed.
#[cfg(unix)]
pub fn active_window() -> isize {
    0
}

/// `Date modified` for one path as the file manager prints it: the local clock
/// in 12-hour form (`3/26/2025 10:49 pm`), together with the raw timestamp so
/// the list can sort on the real value instead of the text.
///
/// Both halves are read on a worker thread; a path that vanished since the scan
/// simply has no date.
pub fn modified(path: &str) -> Option<(std::time::SystemTime, String)> {
    let stamp = std::fs::metadata(path).ok()?.modified().ok()?;
    let label = format_local(stamp)?;
    Some((stamp, label))
}

/// A timestamp in the same `3/26/2025 10:49 pm` form, for dates that came from
/// the scan itself rather than from a file the list just opened.
pub fn format_timestamp(stamp: std::time::SystemTime) -> Option<String> {
    format_local(stamp)
}

/// UTC → the machine's current time zone, then month/day/year and a 12-hour
/// clock. `SYSTEMTIME`/`TzSpecificLocalTime` are used rather than arithmetic
/// because they follow the zone (and its daylight-saving rules) Windows has
/// configured for the user.
#[cfg(windows)]
fn format_local(stamp: std::time::SystemTime) -> Option<String> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

    let since = stamp
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    // 100 ns ticks between 1601-01-01 (FILETIME epoch) and 1970-01-01.
    let ticks =
        (since.as_secs() + 11_644_473_600) * 10_000_000 + u64::from(since.subsec_nanos()) / 100;
    let file_time = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let empty = SYSTEMTIME {
        wYear: 0,
        wMonth: 0,
        wDayOfWeek: 0,
        wDay: 0,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
    };
    let (mut utc, mut local) = (empty, empty);
    // SAFETY: both structures are stack values that outlive the calls, and a
    // null time-zone pointer means "the current zone".
    unsafe {
        if FileTimeToSystemTime(&file_time, &mut utc) == 0 {
            return None;
        }
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return None;
        }
    }

    let hour = match local.wHour % 12 {
        0 => 12,
        other => other,
    };
    let suffix = if local.wHour < 12 { "am" } else { "pm" };
    Some(format!(
        "{}/{}/{} {}:{:02} {}",
        local.wMonth, local.wDay, local.wYear, hour, local.wMinute, suffix
    ))
}

/// The same shape through `localtime_r`: the C library applies the zone and its
/// daylight-saving rules, so the printed clock matches the desktop clock.
#[cfg(unix)]
fn format_local(stamp: std::time::SystemTime) -> Option<String> {
    let seconds = stamp.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is a stack value written by `localtime_r`, and the input is
    // a plain integer.
    if unsafe { libc::localtime_r(&seconds, &mut tm) }.is_null() {
        return None;
    }
    let hour = match tm.tm_hour % 12 {
        0 => 12,
        other => other,
    };
    let suffix = if tm.tm_hour < 12 { "am" } else { "pm" };
    Some(format!(
        "{}/{}/{} {}:{:02} {}",
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_year + 1900,
        hour,
        tm.tm_min,
        suffix
    ))
}

#[cfg(windows)]
static ICON_TARGET: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// The app's own top-level window, for native dialogs and shell menus that want
/// to be owned by it. Zero when Windows has not shown it yet.
#[cfg(windows)]
pub fn main_window() -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

    // SAFETY: the callback only reads the window's process id and visibility and
    // stores into an atomic.
    unsafe {
        ICON_TARGET.store(0, std::sync::atomic::Ordering::SeqCst);
        EnumWindows(Some(find_own_window), 0);
        ICON_TARGET.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[cfg(unix)]
pub fn main_window() -> isize {
    0
}

#[cfg(windows)]
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
#[cfg(windows)]
pub fn apply_window_icon() {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, LoadImageW, SendMessageW, ICON_BIG, ICON_SMALL, IMAGE_ICON,
        LR_DEFAULTCOLOR, SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, WM_SETICON,
    };

    let handle = main_window();
    if handle == 0 {
        return;
    }
    // SAFETY: the Win32 handles are checked for null before use; resource id 1
    // is the icon in `app.rc`.
    unsafe {
        let hwnd = handle as windows_sys::Win32::Foundation::HWND;
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

/// The icon is embedded in the executable on Windows; the other platforms take
/// it from the bundle or the desktop entry instead.
#[cfg(unix)]
pub fn apply_window_icon() {}

/// Ask a yes/no question in a native Windows message box.
///
/// Runs on a background thread so the UI keeps painting while it is open; the
/// default button is **No**, and closing the box answers No.
#[cfg(windows)]
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
#[cfg(windows)]
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

/// A tiny hidden window, created on the calling thread, so that thread can own
/// a popup menu.
///
/// `TrackPopupMenuEx` only services a menu through a window that belongs to the
/// thread showing it, and the frame belongs to the main thread. This hands a
/// worker thread a window of its own; it is never shown. The window procedure
/// only forwards to `DefWindowProcW`, which is all an owner has to do.
#[cfg(windows)]
mod menu_owner {
    use std::cell::RefCell;
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, RegisterClassW, WS_POPUP, WNDCLASSW,
    };

    thread_local! {
        static OWNER: RefCell<Option<HWND>> = RefCell::new(None);
    }

    unsafe extern "system" fn wnd_proc(
        hwnd: HWND,
        message: u32,
        wparam: usize,
        lparam: isize,
    ) -> isize {
        DefWindowProcW(hwnd, message, wparam, lparam)
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The owner window for this thread, created on first use.
    pub fn get() -> HWND {
        OWNER.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(handle) = *slot {
                return handle;
            }
            let class_name = wide("DiskScannerMenuOwner");
            let module = unsafe { GetModuleHandleW(std::ptr::null()) };
            let class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wnd_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: module,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };
            unsafe { RegisterClassW(&class) };
            let handle = unsafe {
                CreateWindowExW(
                    0,
                    class_name.as_ptr(),
                    class_name.as_ptr(),
                    WS_POPUP,
                    0,
                    0,
                    0,
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    module,
                    std::ptr::null(),
                )
            };
            *slot = Some(handle);
            handle
        })
    }
}

/// Show the Windows shell's own context menu for `path`, the way Explorer does.
///
/// The menu is built by the shell (`IContextMenu`), so it carries the verbs the
/// machine actually has — Open, Open with, Properties, Send to, Delete, and any
/// application that registered one. It blocks while it is open, so call it from
/// a background thread; the choice is carried out by the shell itself.
///
/// Returns whether a verb was invoked, so the caller can refresh what it shows.
#[cfg(windows)]
pub fn context_menu(path: &str) -> bool {
    use windows_sys::Win32::Foundation::{HWND, POINT};
    use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows_sys::Win32::UI::Shell::Common::ITEMIDLIST;
    use windows_sys::Win32::UI::Shell::{
        SHBindToParent, SHParseDisplayName, CMINVOKECOMMANDINFOEX, CMF_NORMAL,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreatePopupMenu, DestroyMenu, GetCursorPos, TrackPopupMenuEx, SW_SHOWNORMAL, TPM_RETURNCMD,
        TPM_RIGHTBUTTON,
    };

    /// `CMIC_MASK_UNICODE`: the `W` fields of the invoke structure are used.
    const CMIC_MASK_UNICODE: u32 = 0x0000_4000;

    // {000214E6-0000-0000-C000-000000000046}: IShellFolder.
    const IID_ISHELLFOLDER: windows_sys::core::GUID =
        windows_sys::core::GUID::from_u128(0x0002_14e6_0000_0000_c000_0000_0000_0046);
    // {000214E4-0000-0000-C000-000000000046}: IContextMenu.
    const IID_ICONTEXTMENU: windows_sys::core::GUID =
        windows_sys::core::GUID::from_u128(0x0002_14e4_0000_0000_c000_0000_0000_0046);

    type Release = unsafe extern "system" fn(*mut core::ffi::c_void) -> u32;
    type GetUIObjectOf = unsafe extern "system" fn(
        *mut core::ffi::c_void,
        HWND,
        u32,
        *const *mut ITEMIDLIST,
        *const windows_sys::core::GUID,
        *mut u32,
        *mut *mut core::ffi::c_void,
    ) -> i32;
    type QueryContextMenu =
        unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void, u32, u32, u32, u32)
            -> i32;
    type InvokeCommand =
        unsafe extern "system" fn(*mut core::ffi::c_void, *const CMINVOKECOMMANDINFOEX) -> i32;

    /// A COM object's virtual function slot. Callers pass the index of a method
    /// whose signature they know; the pointer is only read, never stored.
    unsafe fn slot<T: Copy>(object: *mut core::ffi::c_void, index: usize) -> Option<T> {
        if object.is_null() {
            return None;
        }
        // SAFETY: every COM object starts with a pointer to its vtable, and the
        // indices used below are the documented ones for these interfaces.
        let table = *(object as *mut *const usize);
        if table.is_null() {
            return None;
        }
        let entry = *table.add(index);
        if entry == 0 {
            return None;
        }
        Some(std::mem::transmute_copy(&entry))
    }

    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    // The worker thread needs a window of its own to own the popup; the frame
    // belongs to the thread that paints it.
    let owner = menu_owner::get();
    let frame = main_window() as HWND;

    // SAFETY: everything handed to COM outlives the call that uses it, and each
    // interface is released before the apartment is closed. Shell menus are
    // documented to run on an STA thread, which the background thread becomes.
    unsafe {
        if CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) < 0 {
            return false;
        }

        let mut pidl: *mut ITEMIDLIST = std::ptr::null_mut();
        let mut attributes = 0u32;
        if SHParseDisplayName(
            wide.as_ptr(),
            std::ptr::null_mut(),
            &mut pidl,
            0,
            &mut attributes,
        ) < 0
        {
            CoUninitialize();
            return false;
        }

        let mut parent: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut child: *mut ITEMIDLIST = std::ptr::null_mut();
        let bound = SHBindToParent(pidl, &IID_ISHELLFOLDER, &mut parent, &mut child) >= 0;

        let mut menu_object: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut shown = false;
        if bound {
            if let Some(get_ui_object) = slot::<GetUIObjectOf>(parent, 10) {
                let created = get_ui_object(
                    parent,
                    owner,
                    1,
                    &child,
                    &IID_ICONTEXTMENU,
                    std::ptr::null_mut(),
                    &mut menu_object,
                ) >= 0;
                if created {
                    let menu = CreatePopupMenu();
                    let query = slot::<QueryContextMenu>(menu_object, 3);
                    let invoke = slot::<InvokeCommand>(menu_object, 4);
                    if !menu.is_null() {
                        if let (Some(query), Some(invoke)) = (query, invoke) {
                            // Ids 1.. are ours; the shell fills the menu in.
                            let filled = query(menu_object, menu, 0, 1, 0x7fff, CMF_NORMAL);
                            if filled >= 0 {
                                let mut point = POINT { x: 0, y: 0 };
                                GetCursorPos(&mut point);
                                let command = TrackPopupMenuEx(
                                    menu,
                                    TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                    point.x,
                                    point.y,
                                    owner,
                                    std::ptr::null(),
                                );
                                shown = command > 0;
                                if command > 0 {
                                    // The return value is the id the shell gave
                                    // the verb; the offset is what it expects.
                                    let ordinal = (command - 1) as usize;
                                    let mut info: CMINVOKECOMMANDINFOEX =
                                        std::mem::zeroed();
                                    info.cbSize = std::mem::size_of::<CMINVOKECOMMANDINFOEX>()
                                        as u32;
                                    info.fMask = CMIC_MASK_UNICODE;
                                    info.hwnd = frame;
                                    info.lpVerb = ordinal as *const u8;
                                    info.lpVerbW = ordinal as *const u16;
                                    info.nShow = SW_SHOWNORMAL;
                                    info.ptInvoke = point;
                                    invoke(menu_object, &info);
                                }
                            }
                        }
                        DestroyMenu(menu);
                    }
                    if let Some(release) = slot::<Release>(menu_object, 2) {
                        release(menu_object);
                    }
                }
            }
            if let Some(release) = slot::<Release>(parent, 2) {
                release(parent);
            }
        }
        windows_sys::Win32::UI::Shell::ILFree(pidl);
        CoUninitialize();
        shown
    }
}

/// The shell context menu is Windows-only; elsewhere the app keeps its own
/// two-item menu.
#[cfg(unix)]
pub fn context_menu(_path: &str) -> bool {
    false
}

/// Ask a yes/no question in a native dialog before anything is deleted.
///
/// macOS answers through `osascript`; Linux through whichever of the two
/// common dialog tools is installed. With no dialog available the answer is
/// No — refusing is better than deleting without asking.
#[cfg(unix)]
pub fn confirm(_owner: isize, title: &str, text: &str) -> bool {
    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "display dialog \"{text}\" with title \"{title}\" buttons {{\"No\", \"Yes\"}} default button \"No\"",
            text = escape_apple_script(text),
            title = escape_apple_script(title),
        );
        let Ok(output) = Command::new("osascript").args(["-e", &script]).output() else {
            return false;
        };
        return output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("button returned:Yes");
    }

    #[cfg(not(target_os = "macos"))]
    {
        let attempts: [(&str, Vec<&str>); 2] = [
            (
                "zenity",
                vec![
                    "--question",
                    "--no-wrap",
                    "--title",
                    title,
                    "--text",
                    text,
                ],
            ),
            ("kdialog", vec!["--title", title, "--yesno", text]),
        ];
        for (program, args) in attempts {
            match Command::new(program).args(&args).status() {
                Ok(status) => return status.success(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return false,
            }
        }
        false
    }
}

/// Move `path` to the desktop trash.
///
/// The glib and trash-cli tools pick the trash of the volume the item lives on;
/// without either, the freedesktop layout under the data directory is used,
/// which covers the home volume.
#[cfg(unix)]
pub fn recycle(path: &str) -> Result<(), String> {
    let target = std::path::Path::new(path);
    if target.parent().is_none() {
        return Err("refusing to delete a volume root".to_string());
    }
    if !target.exists() {
        return Err(format!("{path} no longer exists"));
    }

    #[cfg(target_os = "macos")]
    {
        let script = format!(
            "tell application \"Finder\" to delete POSIX file \"{}\"",
            escape_apple_script(path)
        );
        return match Command::new("osascript").args(["-e", &script]).status() {
            Ok(status) if status.success() => Ok(()),
            Ok(_) => Err("could not move the item to the Trash".to_string()),
            Err(error) => Err(format!("could not run osascript: {error}")),
        };
    }

    #[cfg(not(target_os = "macos"))]
    {
        for program in ["gio", "trash-put"] {
            let mut command = Command::new(program);
            if program == "gio" {
                command.arg("trash");
            }
            match command.arg(path).status() {
                Ok(status) if status.success() => return Ok(()),
                Ok(_) => {
                    return Err(format!("{program} could not move the item to the trash"));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("could not run {program}: {error}")),
            }
        }
        trash_to_data_dir(path)
    }
}

/// `"...` and `\` are the two characters AppleScript string literals care
/// about, so the paths and titles handed to `osascript` are escaped first.
#[cfg(target_os = "macos")]
fn escape_apple_script(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The freedesktop trash: `files/` holds the item, `info/` a small
/// `.trashinfo` recording where it came from so the desktop can restore it.
#[cfg(all(unix, not(target_os = "macos")))]
fn trash_to_data_dir(path: &str) -> Result<(), String> {
    let source = std::path::Path::new(path);
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".local").join("share"))
        })
        .ok_or("no data directory to keep a trash in")?;
    let files = data.join("Trash").join("files");
    let info = data.join("Trash").join("info");
    for dir in [&files, &info] {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }

    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or("refusing to delete a volume root")?;
    // Two items can share a name; the spec allows `.2`, `.3` suffixes.
    let mut candidate = name.clone();
    let mut counter = 1;
    while files.join(&candidate).exists() || info.join(format!("{candidate}.trashinfo")).exists() {
        counter += 1;
        candidate = format!("{name}.{counter}");
    }
    let moved = files.join(&candidate);
    std::fs::rename(source, &moved)
        .map_err(|error| format!("could not move the item to the trash: {error}"))?;

    let entry = format!(
        "[Trash Info]\nPath={}\nDeletionDate={}\n",
        percent_encode(source),
        timestamp_utc()
    );
    if let Err(error) = std::fs::write(info.join(format!("{candidate}.trashinfo")), entry) {
        // Put the item back rather than leave a trash entry nothing can restore.
        let _ = std::fs::rename(&moved, source);
        return Err(format!("could not record the deletion: {error}"));
    }
    Ok(())
}

/// Percent-encode a path the way `.trashinfo` expects: everything outside the
/// URL-unreserved set becomes `%XX`.
#[cfg(all(unix, not(target_os = "macos")))]
fn percent_encode(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut out = String::new();
    for &byte in path.as_os_str().as_bytes() {
        let plain = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/');
        if plain {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `YYYY-MM-DDThh:mm:ss` in UTC, which is what `DeletionDate` carries.
#[cfg(all(unix, not(target_os = "macos")))]
fn timestamp_utc() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let (days, rest) = ((seconds / 86_400) as i64, seconds % 86_400);
    // Days since 1970-01-01 → calendar date, without pulling in a date crate.
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_position = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_position + 2) / 5 + 1;
    let month = if month_position < 10 {
        month_position + 3
    } else {
        month_position - 9
    };
    let year = year_of_era + era * 400 + if month <= 2 { 1 } else { 0 };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

#[cfg(test)]
mod tests {
    // The only test here drives the trash, which macOS hands to Finder, so on
    // that target the module is empty and nothing needs importing.
    #[cfg(not(target_os = "macos"))]
    use super::*;

    // macOS asks Finder to do the move, which needs an interactive session, so
    // the integration test only runs where the trash works without a desktop.
    #[test]
    #[cfg(not(target_os = "macos"))]
    fn recycles_a_file_and_refuses_a_drive_root() {
        let dir = std::env::temp_dir().join(format!("disk-scanner-sys-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let file = dir.join("recycle-me.txt");
        std::fs::write(&file, b"bye").expect("write fixture");

        recycle(file.to_str().expect("utf8 path")).expect("recycle should succeed");
        assert!(!file.exists(), "file should be out of the working tree");

        let _ = std::fs::remove_dir(&dir);
        #[cfg(windows)]
        assert!(recycle("C:\\").is_err(), "a drive root must be rejected");
        #[cfg(unix)]
        assert!(recycle("/").is_err(), "a volume root must be rejected");
    }
}

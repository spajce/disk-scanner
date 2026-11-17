//! Shell integration: open Explorer at a path, send a folder to the Recycle Bin.

use std::process::Command;
use windows_sys::Win32::UI::Shell::{
    SHFileOperationW, SHFILEOPSTRUCTW, FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION,
    FOF_NOERRORUI, FOF_SILENT,
};

/// Reveal `path` in Explorer, selecting it when it is a file.
pub fn reveal(path: &str) {
    let status = if std::path::Path::new(path).is_dir() {
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

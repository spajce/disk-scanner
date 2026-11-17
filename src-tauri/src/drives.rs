//! Enumerate Windows volumes and their capacity.

use serde::Serialize;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
};
use windows_sys::Win32::System::WindowsProgramming::{
    DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveInfo {
    pub root: String,
    pub label: String,
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn kind_name(raw: u32) -> &'static str {
    match raw {
        DRIVE_REMOVABLE => "removable",
        DRIVE_FIXED => "fixed",
        DRIVE_REMOTE => "network",
        DRIVE_CDROM => "optical",
        DRIVE_RAMDISK => "ramdisk",
        _ => "other",
    }
}

pub fn list() -> Vec<DriveInfo> {
    let mask = unsafe { GetLogicalDrives() };
    let mut drives = Vec::new();

    for index in 0..26u32 {
        if mask & (1 << index) == 0 {
            continue;
        }
        let root = format!("{}:\\", (b'A' + index as u8) as char);
        let root_w = wide(&root);
        let drive_type = unsafe { GetDriveTypeW(root_w.as_ptr()) };
        if drive_type == DRIVE_NO_ROOT_DIR {
            continue;
        }

        let (mut total, mut free) = (0u64, 0u64);
        let mut available_to_caller = 0u64;
        let ok = unsafe {
            GetDiskFreeSpaceExW(
                root_w.as_ptr(),
                &mut available_to_caller,
                &mut total,
                &mut free,
            )
        };
        // Empty optical drives report failure or zero capacity here: skip them.
        if ok == 0 || total == 0 {
            continue;
        }

        let mut label = vec![0u16; 64];
        unsafe {
            GetVolumeInformationW(
                root_w.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
            );
        }
        let label = String::from_utf16_lossy(&label)
            .trim_end_matches('\0')
            .trim()
            .to_string();

        drives.push(DriveInfo {
            root,
            label,
            kind: kind_name(drive_type),
            total,
            free,
        });
    }

    drives
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_at_least_one_fixed_drive_with_capacity() {
        let drives = list();
        assert!(!drives.is_empty(), "no volumes detected");
        let fixed: Vec<_> = drives.iter().filter(|d| d.kind == "fixed").collect();
        assert!(!fixed.is_empty(), "no fixed volumes detected");
        for drive in fixed {
            assert!(drive.total > 0);
            assert!(drive.free <= drive.total);
            assert!(drive.root.ends_with(":\\"));
        }
    }
}

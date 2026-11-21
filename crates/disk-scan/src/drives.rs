//! Enumerate volumes and their capacity.
//!
//! Windows asks the system for its drive letters. Unix has no such notion, so
//! mount points are enumerated and `df` is asked for the numbers — that keeps
//! the crate free of libc plumbing and behaves the same on Linux and macOS.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveInfo {
    pub root: String,
    pub label: String,
    pub kind: &'static str,
    pub total: u64,
    pub free: u64,
}

#[cfg(windows)]
mod platform {
    use super::DriveInfo;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    };
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
    };

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
            // Empty optical drives report failure or zero capacity here: skip.
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
}

#[cfg(unix)]
mod platform {
    use super::DriveInfo;
    use std::path::Path;

    pub fn list() -> Vec<DriveInfo> {
        let mut drives = Vec::new();
        for root in mount_points() {
            // A mount that cannot be measured (a stale share, a vanished
            // volume) is skipped instead of drawn with a meaningless bar.
            let Some((total, free)) = space(&root) else {
                continue;
            };
            if total == 0 {
                continue;
            }
            drives.push(DriveInfo {
                label: label_for(&root),
                kind: "fixed",
                total,
                free,
                root,
            });
        }
        drives.sort_by(|a, b| a.root.cmp(&b.root));
        drives
    }

    /// The volume name under the row: `/home` reads as "home", `/` has none.
    fn label_for(root: &str) -> String {
        if root == "/" {
            return String::new();
        }
        Path::new(root)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    #[cfg(target_os = "macos")]
    fn mount_points() -> Vec<String> {
        // The boot volume, plus everything the system mounted under /Volumes.
        // The aliases there (the familiar "Macintosh HD" link to `/`) are
        // symlinks, and skipping them keeps the boot volume from appearing
        // twice.
        let mut roots = vec!["/".to_string()];
        if let Ok(entries) = std::fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let path = entry.path();
                let is_real_dir = std::fs::symlink_metadata(&path)
                    .map(|meta| meta.is_dir())
                    .unwrap_or(false);
                if is_real_dir {
                    roots.push(path.to_string_lossy().into_owned());
                }
            }
        }
        roots
    }

    #[cfg(not(target_os = "macos"))]
    fn mount_points() -> Vec<String> {
        // `/proc/self/mounts` is the kernel's own list: device, mount point,
        // filesystem type, options. Pseudo filesystems are filtered out; the
        // octal escapes GNU uses for spaces in names are decoded below.
        const REAL: &[&str] = &[
            "ext2", "ext3", "ext4", "xfs", "btrfs", "zfs", "f2fs", "jfs", "reiserfs", "vfat",
            "exfat", "ntfs", "ntfs3", "fuseblk", "hfsplus", "apfs", "iso9660",
        ];

        let mut roots = Vec::new();
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(point) = fields.nth(1) else { continue };
            let Some(fstype) = fields.next() else { continue };
            if !REAL.contains(&fstype) {
                continue;
            }
            let point = unescape_mount(point);
            if !roots.contains(&point) {
                roots.push(point);
            }
        }
        // Bare containers and chroots have no /proc entry to read.
        if roots.is_empty() {
            roots.push("/".to_string());
        }
        roots
    }

    /// `\040` for a space, `\011` tab, `\012` newline, `\134` backslash — the
    /// escapes `/proc/mounts` uses in mount point names.
    #[cfg(not(target_os = "macos"))]
    fn unescape_mount(field: &str) -> String {
        let bytes = field.as_bytes();
        let mut out = String::with_capacity(field.len());
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'\\' && index + 3 < bytes.len() {
                if let Ok(code) = u8::from_str_radix(&field[index + 1..index + 4], 8) {
                    out.push(code as char);
                    index += 4;
                    continue;
                }
            }
            out.push(bytes[index] as char);
            index += 1;
        }
        out
    }

    /// Total and free space, through `df`: `-P` is the POSIX one-line format
    /// and `-k` counts 1024-byte blocks, so both platforms answer the same way.
    fn space(root: &str) -> Option<(u64, u64)> {
        let output = std::process::Command::new("df")
            .arg("-Pk")
            .arg(root)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let line = text.lines().skip(1).find(|line| !line.trim().is_empty())?;
        // Filesystem, 1024-blocks, used, available, capacity, mount point. The
        // filesystem name may contain spaces, so the numbers are read in order
        // rather than by position.
        let mut numbers = line
            .split_whitespace()
            .filter_map(|field| field.parse::<u64>().ok());
        let blocks = numbers.next()?;
        let _used = numbers.next()?;
        let available = numbers.next()?;
        Some((blocks.saturating_mul(1024), available.saturating_mul(1024)))
    }
}

/// Volume list for the current platform.
pub use platform::list;

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
            #[cfg(windows)]
            assert!(drive.root.ends_with(":\\"));
            #[cfg(unix)]
            assert!(drive.root.starts_with('/'));
        }
    }
}

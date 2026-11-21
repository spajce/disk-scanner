//! Parallel directory walker + tree compaction.
//!
//! The walker produces a full `RawNode` tree (one node per directory, one leaf
//! per file). Walking a whole system drive yields millions of leaves, far too
//! many to ship over IPC or draw as SVG arcs, so `to_dto` compacts the tree:
//!
//! * children are sorted by size and capped (`MAX_CHILDREN`),
//! * anything below an absolute floor (fraction of the scanned total) or far
//!   below its parent is folded into a single "(n more)" pseudo node,
//! * single-directory chains (`a/b/c`) are merged into one arc,
//! * a global node budget guarantees a bounded payload.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

use rayon::prelude::*;
use serde::Serialize;

/// `FILE_ATTRIBUTE_REPARSE_POINT` — junctions, mount points and friends.
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
/// Entries whose on-disk footprint can be smaller (or larger) than their
/// logical length: sparse files, NTFS-compressed files, and cloud
/// placeholders that only hold metadata locally.
#[cfg(windows)]
const TIGHTLY_PACKED_ATTRIBUTES: u32 = 0x0200 | 0x0800 | 0x0040_0000 | 0x1000;
/// Only files this big are checked for hard links: opening a handle costs about
/// as much as listing a directory, and linking a 40 KiB file is not worth it.
const DEDUPE_FLOOR: u64 = 8 * 1024 * 1024;

/// How deep the compacted payload may go.
const MAX_DEPTH: usize = 16;
/// Hard cap of kept children per node; the rest is aggregated.
const MAX_CHILDREN: usize = 512;
/// Always keep this many of the biggest children, however small they are.
const MIN_TOP: usize = 10;
/// Absolute floor: fraction of the scanned total a slice needs to be kept.
const ABS_FLOOR: f64 = 0.000_002;
/// Relative floor: fraction of its parent a slice needs to be kept.
const REL_FLOOR: f64 = 0.000_02;
/// Global budget for kept nodes in the payload.
const NODE_BUDGET: usize = 600_000;
/// Leftover this small (fraction of the folder) is folded into the folder
/// itself instead of becoming an unnamed "(n smaller items)" slice.
const REST_FLOOR: f64 = 0.03;
/// Files smaller than this are never drawn as their own arc.
const FILE_FLOOR: u64 = 64 * 1024;
/// At most this many file arcs per directory (the biggest ones).
const MAX_FILE_SLICES: usize = 32;

/// Live counters shared between the walker threads and the progress emitter.
pub struct ScanCounters {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    current: Mutex<String>,
    visited: AtomicU64,
    /// Identity of the large files already counted, so a hard link does not
    /// add its bytes twice when both names are walked.
    links: Mutex<std::collections::HashSet<(u64, u64)>>,
}

impl Default for ScanCounters {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanCounters {
    pub fn new() -> Self {
        Self {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            current: Mutex::new(String::new()),
            visited: AtomicU64::new(0),
            links: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// `(files, dirs, bytes, errors, current_path)` — a cheap, torn snapshot.
    pub fn snapshot(&self) -> (u64, u64, u64, u64, String) {
        let current = self
            .current
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_default();
        (
            self.files.load(Ordering::Relaxed),
            self.dirs.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            current,
        )
    }
}

/// One directory (or file leaf) of the raw, uncompacted tree.
#[derive(Debug)]
pub struct RawNode {
    pub name: String,
    /// Full display path. Only the scan root fills this in: every other path is
    /// rebuilt from its parent while compacting, so a volume with hundreds of
    /// thousands of folders does not allocate a string per directory.
    pub path: String,
    pub size: u64,
    pub files: u64,
    /// Files sitting directly in this directory (not in subdirectories).
    pub direct_files: u64,
    pub dirs: u64,
    pub is_dir: bool,
    pub children: Vec<RawNode>,
}

/// Compact, serialisable node sent to the webview.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeDto {
    pub name: String,
    /// Empty for aggregated `(n more)` nodes.
    pub path: String,
    pub size: u64,
    pub files: u64,
    pub dirs: u64,
    /// `dir` | `file` | `other`
    pub kind: &'static str,
    pub children: Vec<NodeDto>,
}

/// Walk `root` and return the raw tree.
pub fn scan(root: &Path, cancel: &AtomicBool, counters: &ScanCounters) -> Result<RawNode, String> {
    scan_with(root, cancel, None, counters)
}

/// Walk `root` like [`scan`], but hold still while `pause` is set.
///
/// Pausing keeps the walker's counters and partial tree alive, so clearing the
/// flag continues the same walk instead of starting it over. `cancel` still
/// wins: it releases paused walkers so the scan can unwind.
pub fn scan_pausable(
    root: &Path,
    cancel: &AtomicBool,
    pause: &AtomicBool,
    counters: &ScanCounters,
) -> Result<RawNode, String> {
    scan_with(root, cancel, Some(pause), counters)
}

fn scan_with(
    root: &Path,
    cancel: &AtomicBool,
    pause: Option<&AtomicBool>,
    counters: &ScanCounters,
) -> Result<RawNode, String> {
    let meta = std::fs::metadata(root)
        .map_err(|err| format!("Cannot open {}: {}", root.display(), err))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    Ok(walk(root, cancel, pause, counters, 0))
}

/// What one directory listing turned up, without touching the filesystem again.
struct Listing {
    subdirs: Vec<PathBuf>,
    /// Only files at or above [`FILE_FLOOR`], so the caller never allocates for
    /// entries that can never become an arc.
    files: Vec<(u64, PathBuf)>,
    files_seen: u64,
    bytes: u64,
    errors: u64,
}

/// Per-worker scratch space. A system drive has half a million directories,
/// and a fresh 64 KiB read buffer for each of them would be a lot of `malloc`
/// traffic on top of the walk itself.
#[cfg(windows)]
#[derive(Default)]
struct Scratch {
    /// A path as wide characters, prefix already applied, no NUL yet.
    wide: Vec<u16>,
}

#[cfg(windows)]
thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::default());
}

/// `C:\x` -> `\\?\C:\x`, and `\\server\share` -> `\\?\UNC\server\share`,
/// written straight into a reusable wide buffer.
#[cfg(windows)]
fn push_verbatim_wide(path: &Path, out: &mut Vec<u16>) {
    use std::os::windows::ffi::OsStrExt;

    out.clear();
    let mut units = path.as_os_str().encode_wide();
    let first = units.next();
    let second = units.next();
    if first == Some(b'\\' as u16) && second == Some(b'\\' as u16) {
        match units.next() {
            // Already in `\\?\` form.
            Some(unit) if unit == b'?' as u16 => {
                out.extend_from_slice(&[b'\\' as u16, b'\\' as u16, b'?' as u16]);
                out.extend(units);
            }
            // A UNC path: the verbatim prefix has to spell out `UNC`.
            Some(unit) => {
                out.extend(r"\\?\UNC\".encode_utf16());
                out.extend_from_slice(&[b'\\' as u16, b'\\' as u16, unit]);
                out.extend(units);
            }
            None => out.extend(r"\\?\".encode_utf16()),
        }
    } else {
        out.extend(r"\\?\".encode_utf16());
        out.extend(first);
        out.extend(second);
        out.extend(units);
    }
}

/// `true` when this file's bytes have not been counted yet, `false` when it is
/// another name for a file already walked. Files with a single link are never
/// remembered, so the set only ever holds the genuinely shared ones.
#[cfg(windows)]
fn count_once_path(
    path: &Path,
    links: &Mutex<std::collections::HashSet<(u64, u64)>>,
    wide: &mut Vec<u16>,
) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    push_verbatim_wide(path, wide);
    wide.push(0);
    // No access rights: the metadata is available from the handle alone.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return true;
    }
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) } != 0;
    unsafe { CloseHandle(handle) };
    if !ok || info.nNumberOfLinks <= 1 {
        return true;
    }

    let identity = (
        info.dwVolumeSerialNumber as u64,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
    );
    match links.lock() {
        Ok(mut set) => set.insert(identity),
        // A poisoned lock means another thread panicked; keep the bytes rather
        // than silently dropping them from the totals.
        Err(_) => true,
    }
}

/// Size a file actually occupies on the volume: sparse and compressed files
/// take less, cloud placeholders take almost nothing until they are downloaded.
/// Unix reports the same number as `st_blocks`, so only Windows needs this.
#[cfg(windows)]
fn allocated_size(path: &Path, logical: u64, wide: &mut Vec<u16>) -> u64 {
    use windows_sys::Win32::Storage::FileSystem::GetCompressedFileSizeW;

    push_verbatim_wide(path, wide);
    wide.push(0);
    let mut high: u32 = 0;
    let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
    if low == u32::MAX && high == u32::MAX {
        // INVALID_FILE_SIZE, e.g. a file that vanished between listing and this
        // call — fall back to the logical length rather than dropping its bytes.
        return logical;
    }
    ((high as u64) << 32) | low as u64
}

/// Read a directory through `FindFirstFileExW`.
///
/// `FindExInfoBasic` skips the 8.3 short name, which NTFS would otherwise have
/// to synthesise for every entry, and `FIND_FIRST_EX_LARGE_FETCH` lets the API
/// pull a bigger batch out of the filesystem per round trip. Together they are
/// the documented way to make a listing cheaper; `std::fs::read_dir` plus
/// `DirEntry::metadata` would instead cost a second syscall per entry.
#[cfg(windows)]
fn list_dir(dir: &Path, links: &Mutex<std::collections::HashSet<(u64, u64)>>) -> Option<Listing> {
    let mut scratch = SCRATCH.with(|slot| std::mem::take(&mut *slot.borrow_mut()));
    let listing = read_dir_fast(dir, links, &mut scratch);
    SCRATCH.with(|slot| *slot.borrow_mut() = scratch);
    listing
}

#[cfg(windows)]
fn read_dir_fast(
    dir: &Path,
    links: &Mutex<std::collections::HashSet<(u64, u64)>>,
    scratch: &mut Scratch,
) -> Option<Listing> {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
        FILE_ATTRIBUTE_DIRECTORY, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
    };

    // `\\?\C:\dir\*`: FindFirstFile rejects a trailing backslash, so the
    // wildcard supplies the last level.
    push_verbatim_wide(dir, &mut scratch.wide);
    if !matches!(scratch.wide.last(), Some(unit) if *unit == b'\\' as u16) {
        scratch.wide.push(b'\\' as u16);
    }
    scratch.wide.push(b'*' as u16);
    scratch.wide.push(0);

    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstFileExW(
            scratch.wide.as_ptr(),
            FindExInfoBasic,
            std::ptr::addr_of_mut!(data).cast(),
            FindExSearchNameMatch,
            std::ptr::null(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let mut listing = Listing {
        subdirs: Vec::new(),
        files: Vec::new(),
        files_seen: 0,
        bytes: 0,
        errors: 0,
    };
    // Rebuilt for each entry that keeps a path — the packed ones, whose size
    // the volume call needs, and the ones big enough to be drawn. Leaving a
    // stale path in place would pair this file's size with the previous
    // *directory*'s name.
    let mut joined = PathBuf::with_capacity(dir.as_os_str().len() + 64);

    loop {
        let end = data
            .cFileName
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(data.cFileName.len());
        let name = &data.cFileName[..end];
        let is_dot = end == 1 && name[0] == b'.' as u16;
        let is_dot_dot = end == 2 && name[0] == b'.' as u16 && name[1] == b'.' as u16;

        if end > 0 && !is_dot && !is_dot_dot {
            let attributes = data.dwFileAttributes;
            if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                // Symlinks, junctions, mount points and dedup stubs are
                // skipped whole: following them double-counts bytes
                // (`Users\All Users` -> `ProgramData`) and can walk in
                // circles.
                if attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
                    joined.clear();
                    joined.push(dir);
                    joined.push(OsString::from_wide(name));
                    listing.subdirs.push(joined.clone());
                }
            } else {
                // A file. Reparse-point files (dedup stubs, file symlinks) are
                // counted like any other entry: their reported size is the
                // logical size Explorer shows.
                let mut size = ((data.nFileSizeHigh as u64) << 32) | data.nFileSizeLow as u64;
                // Sparse, NTFS-compressed and cloud placeholder files hold less
                // on the disk than their logical length; ask the volume for the
                // allocated size of just those, which are rare enough to keep
                // the extra call off the hot path.
                let packed = attributes & TIGHTLY_PACKED_ATTRIBUTES != 0;
                if packed || size >= FILE_FLOOR {
                    joined.clear();
                    joined.push(dir);
                    joined.push(OsString::from_wide(name));
                    if packed {
                        size = allocated_size(&joined, size, &mut scratch.wide);
                    }
                }
                // A hard link is the same bytes under two names, so only the
                // first name counts. Opening a handle costs about as much as
                // listing a directory, so this only runs for files big enough
                // for a link to distort the chart — always above FILE_FLOOR, so
                // the path above exists.
                let shared =
                    size >= DEDUPE_FLOOR && !count_once_path(&joined, links, &mut scratch.wide);
                if !shared {
                    listing.files_seen += 1;
                    listing.bytes += size;
                    if size >= FILE_FLOOR {
                        listing.files.push((size, joined.clone()));
                    }
                }
                joined.clear();
            }
        }

        if unsafe { FindNextFileW(handle, &mut data) } == 0 {
            break;
        }
    }
    unsafe { FindClose(handle) };

    Some(listing)
}

/// Read a directory with `read_dir`.
///
/// A Unix listing carries no size, so every entry costs one `stat` — the same
/// price `du` pays. That buys the on-disk size (`st_blocks`), which is what the
/// app reports: a sparse file counts the space it occupies, not what it claims.
#[cfg(unix)]
fn list_dir(dir: &Path, links: &Mutex<std::collections::HashSet<(u64, u64)>>) -> Option<Listing> {
    use std::os::unix::fs::MetadataExt;

    let mut listing = Listing {
        subdirs: Vec::new(),
        files: Vec::new(),
        files_seen: 0,
        bytes: 0,
        errors: 0,
    };

    for entry in std::fs::read_dir(dir).ok()? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                listing.errors += 1;
                continue;
            }
        };
        let path = entry.path();
        // `symlink_metadata` never follows the link: a link back to a parent
        // cannot trap the walk, and a link to a big file is not counted twice.
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(_) => {
                listing.errors += 1;
                continue;
            }
        };
        let kind = meta.file_type();
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            listing.subdirs.push(path);
            continue;
        }
        if !kind.is_file() {
            // Sockets, FIFOs and device nodes hold no meaningful bytes.
            continue;
        }

        // `st_blocks` counts 512-byte units on every Unix, so this is the space
        // the file really occupies. Filesystems that keep small files inline
        // report zero blocks; fall back to the logical length there.
        let allocated = meta.blocks().saturating_mul(512);
        let size = if allocated == 0 { meta.len() } else { allocated };
        // A hard link is the same bytes under two names, so only the first name
        // counts. `st_nlink` is in the `stat` already paid for, so this costs
        // nothing extra here.
        if size >= DEDUPE_FLOOR && !count_once(&meta, links) {
            continue;
        }
        listing.files_seen += 1;
        listing.bytes += size;
        if size >= FILE_FLOOR {
            listing.files.push((size, path));
        }
    }

    Some(listing)
}

/// `true` when this file's bytes have not been counted yet, `false` when it is
/// another name for a file already walked. `st_nlink` is in the `stat` the walk
/// already paid for, so a hard link costs the same as any other file here.
#[cfg(unix)]
fn count_once(
    meta: &std::fs::Metadata,
    links: &Mutex<std::collections::HashSet<(u64, u64)>>,
) -> bool {
    use std::os::unix::fs::MetadataExt;

    if meta.nlink() <= 1 {
        return true;
    }
    match links.lock() {
        Ok(mut set) => set.insert((meta.dev(), meta.ino())),
        // A poisoned lock means another thread panicked; keep the bytes rather
        // than silently dropping them from the totals.
        Err(_) => true,
    }
}


/// Block while `pause` is set. Polling lets a cancel release a paused walk
/// without waiting for a resume; the loop costs one wakeup per idle thread.
fn wait_while_paused(cancel: &AtomicBool, pause: Option<&AtomicBool>) {
    let Some(pause) = pause else { return };
    while pause.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn walk(
    dir: &Path,
    cancel: &AtomicBool,
    pause: Option<&AtomicBool>,
    counters: &ScanCounters,
    depth: usize,
) -> RawNode {
    let mut node = RawNode {
        name: node_name(dir),
        // Only the root keeps its full path; the rest are rebuilt from their
        // parent during compaction. A system drive has hundreds of thousands
        // of directories, and a path string each is a lot of allocation for
        // something only the kept arcs ever need.
        path: if depth == 0 {
            display_path(dir)
        } else {
            String::new()
        },
        size: 0,
        files: 0,
        direct_files: 0,
        dirs: 0,
        is_dir: true,
        children: Vec::new(),
    };

    if depth > MAX_DEPTH || cancel.load(Ordering::Relaxed) {
        return node;
    }

    // Stop parks the walker here, at a directory boundary. The partial tree and
    // the counters stay alive, so Continue resumes the same walk.
    wait_while_paused(cancel, pause);
    if cancel.load(Ordering::Relaxed) {
        return node;
    }

    // Publish the directory we are in now and then, for the progress bar.
    let seen = counters.visited.fetch_add(1, Ordering::Relaxed);
    if seen % 32 == 0 {
        if let Ok(mut slot) = counters.current.lock() {
            slot.clear();
            write_display_path(dir, &mut slot);
        }
    }

    // One pass gives names, attributes and sizes together, with hard links
    // resolved as the entries are read.
    let listing = match list_dir(dir, &counters.links) {
        Some(listing) => listing,
        None => {
            // Not readable (protected, in use, ...) — skip silently but count it.
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return node;
        }
    };

    node.size = listing.bytes;
    node.files = listing.files_seen;
    node.direct_files = listing.files_seen;

    // Bumped once per directory instead of once per entry: a system drive has
    // millions of files and every atomic there is a locked instruction.
    counters.dirs.fetch_add(listing.subdirs.len() as u64, Ordering::Relaxed);
    counters.files.fetch_add(listing.files_seen, Ordering::Relaxed);
    counters.bytes.fetch_add(listing.bytes, Ordering::Relaxed);
    if listing.errors > 0 {
        counters.errors.fetch_add(listing.errors, Ordering::Relaxed);
    }

    // Split whenever a directory actually branches. Rayon steals the work, so
    // deep trees get walked by every core instead of only the first few levels;
    // a two-entry directory costs a handful of microseconds to split, which is
    // nothing next to the two directory reads it then does in parallel.
    let mut children: Vec<RawNode> = if listing.subdirs.len() >= 4 {
        listing
            .subdirs
            .par_iter()
            .map(|subdir| walk(subdir, cancel, pause, counters, depth + 1))
            .collect()
    } else {
        listing
            .subdirs
            .iter()
            .map(|subdir| walk(subdir, cancel, pause, counters, depth + 1))
            .collect()
    };

    // Only the biggest files earn their own arc; everything else stays folded
    // into the parent's size, which keeps the payload small on file-heavy trees.
    let mut files = listing.files;
    files.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    for (size, path) in files.into_iter().take(MAX_FILE_SLICES) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        children.push(RawNode {
            name,
            path: String::new(),
            size,
            files: 1,
            direct_files: 1,
            dirs: 0,
            is_dir: false,
            children: Vec::new(),
        });
    }

    // File sizes were already added above from the directory listing itself, so
    // only subdirectories contribute here — adding the file leaves a second time
    // would double every byte the tree reports.
    for child in &children {
        if child.is_dir {
            node.size += child.size;
            node.files += child.files;
            node.dirs += child.dirs + 1;
        }
    }
    node.children = children;
    node
}

/// Compact a raw tree into the payload the frontend draws.
pub fn to_dto(root: &RawNode, total: u64) -> NodeDto {
    let mut budget = NODE_BUDGET;
    convert(root, total, 0, &mut budget, "")
}

fn convert(node: &RawNode, total: u64, depth: usize, budget: &mut usize, parent: &str) -> NodeDto {
    if !node.is_dir {
        *budget = budget.saturating_sub(1);
        return NodeDto {
            name: node.name.clone(),
            path: child_path(parent, &node.name),
            size: node.size,
            files: 1,
            dirs: 0,
            kind: "file",
            children: Vec::new(),
        };
    }

    // Collapse `a/b/c` chains: a folder holding a single folder and no file of
    // its own is just a longer name — drawing it as extra rings is noise. The
    // path follows the chain, so the node still points at the folder the label
    // actually names.
    let mut current = node;
    let mut name = node.name.clone();
    let mut path = if parent.is_empty() {
        node.path.clone()
    } else {
        child_path(parent, &node.name)
    };
    while depth > 0
        && current.direct_files == 0
        && current.children.len() == 1
        && current.children[0].is_dir
        && depth < MAX_DEPTH
    {
        current = &current.children[0];
        name.push('/');
        name.push_str(&current.name);
        path = child_path(&path, &current.name);
    }

    let mut kids: Vec<&RawNode> = current.children.iter().collect();
    kids.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    let abs_floor = ((total as f64) * ABS_FLOOR).max(1.0) as u64;
    let rel_floor = ((current.size as f64) * REL_FLOOR) as u64;

    let mut kept: Vec<NodeDto> = Vec::new();
    // The node budget is shared out by *size* rather than evenly, and it is
    // re-shared after every child: an even split starves deep branches (each
    // level divides again, so `AppData\Local` ended up with a dozen nodes and
    // folded everything else into one "(n smaller items)" block), while letting
    // a subtree take a share proportional to its bytes keeps the detail where
    // the space actually is.
    let mut remaining = *budget;
    let mut left_weight: u64 = kids
        .iter()
        .enumerate()
        .filter(|(index, child)| {
            depth < MAX_DEPTH
                && (*index < MIN_TOP || child.size >= abs_floor.max(rel_floor))
        })
        .map(|(_, child)| child.size.max(1))
        .sum();

    for (index, child) in kids.iter().enumerate() {
        let wanted = depth < MAX_DEPTH
            && remaining > 0
            && kept.len() < MAX_CHILDREN
            && (index < MIN_TOP || child.size >= abs_floor.max(rel_floor));
        if !wanted {
            continue;
        }

        let weight = child.size.max(1);
        let share_of_pool = weight as f64 / left_weight.max(1) as f64;
        let mut share = ((remaining as f64 * share_of_pool) as usize).max(1);
        let granted = share;
        kept.push(convert(child, total, depth + 1, &mut share, &path));
        remaining = remaining.saturating_sub(granted.saturating_sub(share));
        left_weight = left_weight.saturating_sub(weight);
    }

    *budget = remaining.saturating_sub(1);

    // Everything without an arc of its own — children below the floors, children
    // over MAX_CHILDREN, and loose files under FILE_FLOOR — still has to be
    // visible, otherwise the arcs would not add up to the folder in the middle.
    // When that leftover is a rounding detail we fold it into the folder itself
    // instead of drawing an unnamed "(n more)" block that hides real names.
    let kept_size: u64 = kept.iter().map(|child| child.size).sum();
    let kept_files: u64 = kept.iter().map(|child| child.files).sum();
    let kept_dirs: u64 = kept.iter().map(|child| child.dirs).sum();
    let rest_size = current.size.saturating_sub(kept_size);

    // File and folder counts always stay exact — only the byte total of a
    // folder is allowed to shrink to the arcs that are actually drawn, so the
    // panel never claims fewer files than the volume has.
    let mut shown_size = current.size;

    if rest_size > 0 {
        let share = rest_size as f64 / current.size.max(1) as f64;
        if share <= REST_FLOOR && kept_size > 0 {
            shown_size = kept_size;
        } else {
            let rest_children = current.children.len().saturating_sub(kept.len());
            kept.push(NodeDto {
                name: if rest_children > 0 {
                    format!("{} smaller items", rest_children)
                } else {
                    "smaller files".to_string()
                },
                path: String::new(),
                size: rest_size,
                files: current.files.saturating_sub(kept_files),
                dirs: current.dirs.saturating_sub(kept_dirs),
                kind: "other",
                children: Vec::new(),
            });
        }
    }

    kept.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    NodeDto {
        name,
        path,
        size: shown_size,
        files: current.files,
        dirs: current.dirs,
        kind: "dir",
        children: kept,
    }
}

/// Inverse of [`push_verbatim_wide`], for display and for the filesystem calls
/// that take a path back.
fn display_path(path: &Path) -> String {
    let mut out = String::new();
    write_display_path(path, &mut out);
    out
}

/// [`display_path`] into a buffer the caller already owns, so the progress
/// readout can be rewritten without a throwaway string.
fn write_display_path(path: &Path, out: &mut String) {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        out.push_str(r"\\");
        out.push_str(rest);
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        out.push_str(rest);
    } else {
        out.push_str(&text);
    }
}

/// One path level down, using the platform's separator. A root such as `C:\`
/// already ends in one, so it must not be doubled.
fn child_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        return name.to_string();
    }
    let mut out = String::with_capacity(parent.len() + name.len() + 1);
    out.push_str(parent);
    if !out.ends_with(std::path::MAIN_SEPARATOR) {
        out.push(std::path::MAIN_SEPARATOR);
    }
    out.push_str(name);
    out
}

fn node_name(path: &Path) -> String {
    if let Some(name) = path.file_name() {
        let name = name.to_string_lossy().into_owned();
        if !name.is_empty() {
            return name;
        }
    }
    display_path(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::AtomicBool;

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "disk-scanner-test-{}-{}",
                tag,
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            TempTree(dir)
        }

        fn file(&self, rel: &str, bytes: usize) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, vec![b'x'; bytes]).unwrap();
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scan_tree(path: &Path) -> RawNode {
        scan(path, &AtomicBool::new(false), &ScanCounters::new()).unwrap()
    }

    /// What the walk is expected to report for one file: the space it takes on
    /// the volume. That is the logical length on Windows (NTFS reports it
    /// exactly) and the allocated block count on Unix, so a fixture of exact
    /// lengths does not add up to those lengths there.
    #[cfg(unix)]
    fn on_disk(path: &Path) -> u64 {
        use std::os::unix::fs::MetadataExt;

        let meta = fs::metadata(path).expect("fixture file");
        let allocated = meta.blocks().saturating_mul(512);
        if allocated == 0 {
            meta.len()
        } else {
            allocated
        }
    }

    #[cfg(not(unix))]
    fn on_disk(path: &Path) -> u64 {
        fs::metadata(path).expect("fixture file").len()
    }

    #[test]
    fn a_paused_walk_holds_its_counters_and_resumes() {
        let tree = TempTree::new("pause");
        tree.file("a.bin", 1024);
        tree.file("sub/b.bin", 2048);

        let cancel = AtomicBool::new(false);
        let pause = AtomicBool::new(true);
        let counters = ScanCounters::new();

        std::thread::scope(|scope| {
            let walk = scope.spawn(|| {
                scan_pausable(&tree.0, &cancel, &pause, &counters)
                    .expect("a paused scan can still finish")
                    .size
            });

            // The walker must be parked at its first pause gate, not counting.
            std::thread::sleep(std::time::Duration::from_millis(150));
            assert!(!walk.is_finished(), "a paused walk must not run to completion");
            assert_eq!(
                counters.files.load(Ordering::Relaxed),
                0,
                "a paused walk must not count files"
            );

            pause.store(false, Ordering::Relaxed);
            let expected = on_disk(&tree.0.join("a.bin"))
                + on_disk(&tree.0.join("sub").join("b.bin"));
            assert_eq!(walk.join().unwrap(), expected);
        });
    }

    #[test]
    fn sums_file_sizes_recursively() {
        let tree = TempTree::new("sums");
        tree.file("a.bin", 1024);
        tree.file("sub/b.bin", 2048);
        tree.file("sub/deep/c.bin", 4096);

        let expected = on_disk(&tree.0.join("a.bin"))
            + on_disk(&tree.0.join("sub").join("b.bin"))
            + on_disk(&tree.0.join("sub").join("deep").join("c.bin"));
        let raw = scan_tree(&tree.0);
        assert_eq!(raw.size, expected);
        assert_eq!(raw.files, 3);
        assert_eq!(raw.dirs, 2);
    }

    #[test]
    fn large_files_are_not_counted_twice() {
        let tree = TempTree::new("double");
        tree.file("big.bin", 256 * 1024);
        tree.file("sub/huge.bin", 512 * 1024);
        tree.file("sub/tiny.bin", 32);

        let expected = on_disk(&tree.0.join("big.bin"))
            + on_disk(&tree.0.join("sub").join("huge.bin"))
            + on_disk(&tree.0.join("sub").join("tiny.bin"));
        let raw = scan_tree(&tree.0);
        assert_eq!(raw.size, expected, "bytes must be counted once");
        assert_eq!(raw.files, 3, "files must be counted once");

        let dto = to_dto(&raw, raw.size);
        let sum: u64 = dto.children.iter().map(|child| child.size).sum();
        assert_eq!(sum, dto.size, "arcs must add up to the folder total");
    }

    #[test]
    fn drawn_files_keep_their_own_name() {
        let tree = TempTree::new("names");
        // A subdirectory is walked first, so the reusable path buffer is
        // holding its name when the big file below is visited.
        tree.file("folder/inner.bin", 512);
        tree.file("payload.bin", FILE_FLOOR as usize);

        let raw = scan_tree(&tree.0);
        let dto = to_dto(&raw, raw.size);
        for child in &dto.children {
            let last = child
                .path
                .rsplit(std::path::MAIN_SEPARATOR)
                .next()
                .unwrap_or_default();
            assert_eq!(
                child.name, last,
                "a node's path must be built from its own name, not a neighbour's"
            );
        }
        assert!(
            dto.children
                .iter()
                .any(|child| child.kind == "file" && child.name == "payload.bin"),
            "the file big enough to draw keeps its own name"
        );
    }

    #[test]
    fn hard_links_are_counted_once() {
        let tree = TempTree::new("links");
        tree.file("original.bin", DEDUPE_FLOOR as usize);
        // A second name for the same bytes: NTFS reports the same file size for
        // both, but only one of them occupies space on the volume.
        let original = tree.0.join("original.bin");
        std::fs::hard_link(&original, tree.0.join("alias.bin")).unwrap();

        let raw = scan_tree(&tree.0);
        assert_eq!(
            raw.size, DEDUPE_FLOOR,
            "a hard link must not add its bytes twice"
        );
        assert_eq!(raw.files, 1, "a hard link must not add a second file");
    }

    #[test]
    fn missing_directory_is_an_error_not_a_panic() {
        let missing = std::env::temp_dir().join("disk-scanner-does-not-exist-xyz");
        let err = scan(&missing, &AtomicBool::new(false), &ScanCounters::new()).unwrap_err();
        assert!(err.contains("Cannot open"), "unexpected error: {err}");
    }

    #[test]
    fn dto_sorts_children_and_folds_a_tiny_remainder() {
        let tree = TempTree::new("agg");
        tree.file("big/one.bin", 100_000);
        tree.file("mid/two.bin", 50_000);
        // Forty tiny folders: keeping every one of them is fine, they are just
        // slivers — an unnamed "(n smaller items)" block would hide real names.
        for index in 0..40 {
            tree.file(&format!("tiny{index}/x.bin"), 16);
        }

        let raw = scan_tree(&tree.0);
        let dto = to_dto(&raw, raw.size);

        assert_eq!(dto.kind, "dir");
        let sizes: Vec<u64> = dto.children.iter().map(|child| child.size).collect();
        let mut sorted = sizes.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted, "children must be sorted by size");
        assert!(
            !dto.children.iter().any(|child| child.kind == "other"),
            "a rounding remainder is folded into the folder, not drawn"
        );
        let kept_total: u64 = dto.children.iter().map(|child| child.size).sum();
        assert_eq!(kept_total, dto.size, "arcs must add up to the folder total");
    }

    #[test]
    fn dto_aggregates_only_a_large_remainder() {
        let tree = TempTree::new("big-agg");
        // More folders than MAX_CHILDREN can hold: the leftover is far too big
        // to fold into the parent silently, so it has to become a slice.
        for index in 0..(MAX_CHILDREN + 180) {
            tree.file(&format!("item{index:03}/x.bin"), 4096);
        }

        let raw = scan_tree(&tree.0);
        let total = raw.size;
        let dto = to_dto(&raw, total);

        assert!(
            dto.children.len() <= MAX_CHILDREN + 1,
            "the child cap must hold"
        );
        let rest = dto
            .children
            .iter()
            .find(|child| child.kind == "other")
            .expect("a large leftover still gets a slice");
        assert!(rest.name.ends_with("smaller items"), "got {:?}", rest.name);
        let kept_total: u64 = dto.children.iter().map(|child| child.size).sum();
        assert_eq!(kept_total, total, "aggregation must not lose bytes");
    }

    #[test]
    fn dto_collapses_single_child_chains() {
        let tree = TempTree::new("chain");
        tree.file("a/b/c/leaf.bin", 128 * 1024);

        let raw = scan_tree(&tree.0);
        let dto = to_dto(&raw, raw.size);
        let a = dto.children.iter().find(|child| child.name.starts_with("a")).unwrap();
        assert_eq!(a.name, "a/b/c");
        assert_eq!(a.children[0].kind, "file");
    }

    #[test]
    fn dto_never_exceeds_the_node_budget() {
        let tree = TempTree::new("budget");
        // Three levels of ten siblings each => 1000 leaf-ish nodes.
        for a in 0..10 {
            for b in 0..10 {
                tree.file(&format!("a{a}/b{b}/file.bin"), 1024 + a * 16 + b);
            }
        }
        let raw = scan_tree(&tree.0);
        let dto = to_dto(&raw, raw.size);

        fn count(node: &NodeDto) -> usize {
            1 + node.children.iter().map(count).sum::<usize>()
        }
        assert!(count(&dto) <= NODE_BUDGET);
    }
}

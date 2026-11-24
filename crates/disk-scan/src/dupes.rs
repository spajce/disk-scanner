//! Duplicate search: files with identical contents, found without reading the
//! whole disk twice.
//!
//! The scan index already keeps every file of at least a megabyte, with its
//! size. Grouping by size is free and throws away almost everything; the first
//! 64 KiB of each remaining candidate throws away the files that only happen to
//! share a size; only what survives both is read end to end and hashed.

use crate::index::FileHit;
use rayon::prelude::*;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// How much of a file is compared before it is read whole.
pub const PARTIAL_BYTES: u64 = 64 * 1024;

/// FNV-1a, 64 bit. Not a cryptographic hash: the goal is to compare files that a
/// size check and a prefix check already put in the same group, not to defend
/// against someone crafting a collision.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// One set of files whose contents are identical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DupGroup {
    pub size: u64,
    pub copies: Vec<FileHit>,
}

/// The result of a duplicate search.
#[derive(Clone, Debug, Default)]
pub struct DuplicateScan {
    pub groups: Vec<DupGroup>,
    /// Files whose contents were read, for the progress line.
    pub hashed: usize,
    pub cancelled: bool,
}

impl DuplicateScan {
    /// Space that one copy of each duplicate set would give back.
    pub fn wasted(&self) -> u64 {
        self.groups
            .iter()
            .map(|group| group.size * (group.copies.len() as u64 - 1))
            .sum()
    }

    pub fn copies(&self) -> usize {
        self.groups.iter().map(|group| group.copies.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Groups worth the most space first, so the list opens on the expensive
    /// duplicates rather than on a hundred copies of one screenshot.
    fn sort(&mut self) {
        self.groups.sort_by(|left, right| {
            let left_waste = left.size * (left.copies.len() as u64 - 1);
            let right_waste = right.size * (right.copies.len() as u64 - 1);
            right_waste
                .cmp(&left_waste)
                .then_with(|| right.size.cmp(&left.size))
                .then_with(|| left.copies[0].path.cmp(&right.copies[0].path))
        });
    }
}

/// Groups the duplicates among `files`, reporting each read file in `progress`
/// and stopping when `cancel` is raised.
pub fn find(files: &[FileHit], cancel: &AtomicBool, progress: &AtomicU64) -> DuplicateScan {
    // Only files of the same size can be duplicates.
    let mut by_size: HashMap<u64, Vec<usize>> = HashMap::new();
    for (offset, hit) in files.iter().enumerate() {
        if hit.size > 0 {
            by_size.entry(hit.size).or_default().push(offset);
        }
    }
    let mut candidates: Vec<usize> = Vec::new();
    for offsets in by_size.values() {
        if offsets.len() >= 2 {
            candidates.extend_from_slice(offsets);
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    if candidates.is_empty() {
        return DuplicateScan::default();
    }

    // First 64 KiB: a size match is not a content match.
    let prefixes = hash_all(files, &candidates, Some(PARTIAL_BYTES), cancel, progress);
    if cancel.load(Ordering::Relaxed) {
        return DuplicateScan {
            hashed: progress.load(Ordering::Relaxed) as usize,
            cancelled: true,
            ..DuplicateScan::default()
        };
    }
    let mut by_prefix: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (offset, hash) in prefixes {
        if let Some(hash) = hash {
            by_prefix
                .entry((files[offset].size, hash))
                .or_default()
                .push(offset);
        }
    }
    let mut deeper: Vec<usize> = Vec::new();
    for offsets in by_prefix.values() {
        if offsets.len() >= 2 {
            deeper.extend_from_slice(offsets);
        }
    }
    deeper.sort_unstable();
    deeper.dedup();

    // Same size and same start: read the rest. Most pairs never get here.
    let full = hash_all(files, &deeper, None, cancel, progress);
    if cancel.load(Ordering::Relaxed) {
        return DuplicateScan {
            hashed: progress.load(Ordering::Relaxed) as usize,
            cancelled: true,
            ..DuplicateScan::default()
        };
    }
    let mut by_content: HashMap<(u64, u64), Vec<usize>> = HashMap::new();
    for (offset, hash) in full {
        if let Some(hash) = hash {
            by_content
                .entry((files[offset].size, hash))
                .or_default()
                .push(offset);
        }
    }

    let mut scan = DuplicateScan {
        hashed: progress.load(Ordering::Relaxed) as usize,
        ..DuplicateScan::default()
    };
    for ((size, _), offsets) in by_content {
        if offsets.len() < 2 {
            continue;
        }
        let mut copies: Vec<FileHit> = offsets.iter().map(|&offset| files[offset].clone()).collect();
        copies.sort_by(|left, right| left.path.cmp(&right.path));
        scan.groups.push(DupGroup { size, copies });
    }
    scan.sort();
    scan
}

fn hash_all(
    files: &[FileHit],
    offsets: &[usize],
    limit: Option<u64>,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) -> Vec<(usize, Option<u64>)> {
    offsets
        .par_iter()
        .map(|&offset| {
            if cancel.load(Ordering::Relaxed) {
                return (offset, None);
            }
            let hash = hash_file(Path::new(&files[offset].path), limit, cancel);
            progress.fetch_add(1, Ordering::Relaxed);
            (offset, hash)
        })
        .collect()
}

/// Hashes a file's contents, or its first `limit` bytes. A file that cannot be
/// read, or a cancelled search, is reported as `None` and left out of the
/// groups rather than failing the whole search.
fn hash_file(path: &Path, limit: Option<u64>, cancel: &AtomicBool) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut left = limit.unwrap_or(u64::MAX);
    let mut hash = FNV_OFFSET;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        let want = buffer.len().min(left.min(usize::MAX as u64) as usize);
        if want == 0 {
            break;
        }
        let read = match file.read(&mut buffer[..want]) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return None,
        };
        for &byte in &buffer[..read] {
            hash = (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
        }
        left -= read as u64;
    }
    Some(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-scanner-dupes-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("scratch folder");
        dir
    }

    fn write(path: &Path, bytes: &[u8]) {
        fs::write(path, bytes).expect("write fixture");
    }

    fn hit(path: &Path) -> FileHit {
        FileHit {
            path: path.to_string_lossy().into_owned(),
            size: fs::metadata(path).map(|meta| meta.len()).unwrap_or(0),
            modified: -1,
        }
    }

    #[test]
    fn groups_identical_files_and_leaves_the_rest_alone() {
        let dir = scratch();
        let one = dir.join("one.bin");
        let two = dir.join("two.bin");
        let three = dir.join("three.bin");
        let other = dir.join("other.bin");
        write(&one, &[7u8; 200_000]);
        write(&two, &[7u8; 200_000]);
        write(&three, &[7u8; 200_000]);
        // Same size as the copies above, different content from its second byte.
        write(&other, &{
            let mut bytes = vec![7u8; 200_000];
            bytes[1] = 9;
            bytes
        });
        let files: Vec<FileHit> = [&one, &two, &three, &other]
            .iter()
            .map(|path| hit(path))
            .collect();
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let scan = find(&files, &cancel, &progress);
        assert!(!scan.cancelled);
        assert_eq!(scan.groups.len(), 1, "one set of three copies");
        let group = &scan.groups[0];
        assert_eq!(group.copies.len(), 3);
        assert_eq!(group.size, 200_000);
        assert_eq!(scan.copies(), 3);
        assert_eq!(scan.wasted(), 400_000);
        // The differing file was read for its prefix only, the copies whole.
        assert!(scan.hashed >= 4);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn files_of_different_sizes_are_not_candidates() {
        let dir = scratch();
        let one = dir.join("a.txt");
        let two = dir.join("b.txt");
        write(&one, b"short");
        write(&two, b"a longer file");
        let files = vec![hit(&one), hit(&two)];
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let scan = find(&files, &cancel, &progress);
        assert!(scan.is_empty());
        assert_eq!(progress.load(Ordering::Relaxed), 0);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_file_is_skipped_instead_of_failing_the_search() {
        let dir = scratch();
        let one = dir.join("one.bin");
        let two = dir.join("two.bin");
        let missing = dir.join("gone.bin");
        write(&one, &[3u8; 5000]);
        write(&two, &[3u8; 5000]);
        let mut files = vec![hit(&one), hit(&two)];
        files.push(FileHit {
            path: missing.to_string_lossy().into_owned(),
            size: 5000,
            modified: -1,
        });
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let scan = find(&files, &cancel, &progress);
        assert_eq!(scan.groups.len(), 1);
        assert_eq!(scan.groups[0].copies.len(), 2);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cancelling_stops_the_search_early() {
        let dir = scratch();
        let one = dir.join("one.bin");
        let two = dir.join("two.bin");
        write(&one, &[1u8; 300_000]);
        write(&two, &[1u8; 300_000]);
        let files = vec![hit(&one), hit(&two)];
        let cancel = AtomicBool::new(true);
        let progress = AtomicU64::new(0);
        let scan = find(&files, &cancel, &progress);
        assert!(scan.cancelled);
        assert!(scan.is_empty());
        fs::remove_dir_all(&dir).ok();
    }
}

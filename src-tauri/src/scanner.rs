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

use rayon::prelude::*;
use serde::Serialize;
use std::os::windows::fs::MetadataExt;

/// `FILE_ATTRIBUTE_REPARSE_POINT` — junctions, mount points and friends.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;

/// How deep the compacted payload may go.
const MAX_DEPTH: usize = 16;
/// Hard cap of kept children per node; the rest is aggregated.
const MAX_CHILDREN: usize = 64;
/// Always keep this many of the biggest children, however small they are.
const MIN_TOP: usize = 10;
/// Absolute floor: fraction of the scanned total a slice needs to be kept.
const ABS_FLOOR: f64 = 0.000_08;
/// Relative floor: fraction of its parent a slice needs to be kept.
const REL_FLOOR: f64 = 0.001;
/// Global budget for kept nodes in the payload.
const NODE_BUDGET: usize = 80_000;
/// Files smaller than this are never drawn as their own arc.
const FILE_FLOOR: u64 = 64 * 1024;
/// At most this many file arcs per directory (the biggest ones).
const MAX_FILE_SLICES: usize = 8;

/// Live counters shared between the walker threads and the progress emitter.
pub struct ScanCounters {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub bytes: AtomicU64,
    pub errors: AtomicU64,
    current: Mutex<String>,
    visited: AtomicU64,
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
    let meta = std::fs::metadata(root)
        .map_err(|err| format!("Cannot open {}: {}", root.display(), err))?;
    if !meta.is_dir() {
        return Err(format!("{} is not a folder", root.display()));
    }
    Ok(walk(root, cancel, counters, 0))
}

fn walk(dir: &Path, cancel: &AtomicBool, counters: &ScanCounters, depth: usize) -> RawNode {
    let mut node = RawNode {
        name: node_name(dir),
        path: display_path(dir),
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

    // Publish the directory we are in now and then, for the progress bar.
    let seen = counters.visited.fetch_add(1, Ordering::Relaxed);
    if seen % 32 == 0 {
        if let Ok(mut slot) = counters.current.lock() {
            slot.clear();
            slot.push_str(&node.path);
        }
    }

    let reader = match std::fs::read_dir(verbatim(dir)) {
        Ok(reader) => reader,
        Err(_) => {
            // Not readable (protected, in use, ...) — skip silently but count it.
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return node;
        }
    };

    let mut subdirs: Vec<PathBuf> = Vec::new();
    let mut files: Vec<(String, String, u64)> = Vec::new();

    for entry in reader {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        // `file_type` comes from the directory entry itself: cheap and it does
        // not follow reparse points, which is exactly what we need to spot
        // junctions/symlinks before recursing into a cycle.
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Symlinks and every other kind of reparse point (junctions, mount
        // points, dedup stubs) are skipped wholesale: following them
        // double-counts bytes (`Users\All Users` -> `ProgramData`) and can walk
        // in circles. `DirEntry::metadata` does not traverse the link itself.
        if file_type.is_symlink() {
            continue;
        }

        if file_type.is_dir() {
            let reparse = entry
                .metadata()
                .map(|meta| meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
                .unwrap_or(false);
            if reparse {
                continue;
            }
            counters.dirs.fetch_add(1, Ordering::Relaxed);
            subdirs.push(entry.path());
        } else {
            let len = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            node.size += len;
            node.files += 1;
            node.direct_files += 1;
            if len >= FILE_FLOOR {
                files.push((
                    entry.file_name().to_string_lossy().into_owned(),
                    entry.path().to_string_lossy().into_owned(),
                    len,
                ));
            }
            counters.files.fetch_add(1, Ordering::Relaxed);
            counters.bytes.fetch_add(len, Ordering::Relaxed);
        }
    }

    // Parallelise where there is actually something to split. Deep single-child
    // chains stay sequential: spawning tasks there costs more than it saves.
    let mut children: Vec<RawNode> = if subdirs.len() > 1 && depth < 6 {
        subdirs
            .par_iter()
            .map(|subdir| walk(subdir, cancel, counters, depth + 1))
            .collect()
    } else {
        subdirs
            .iter()
            .map(|subdir| walk(subdir, cancel, counters, depth + 1))
            .collect()
    };

    // Only the biggest files earn their own arc; everything else stays folded
    // into the parent's size, which keeps the payload small on file-heavy trees.
    files.sort_by(|a, b| b.2.cmp(&a.2));
    for (name, path, size) in files.into_iter().take(MAX_FILE_SLICES) {
        children.push(RawNode {
            name,
            path,
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
    convert(root, total, 0, &mut budget)
}

fn convert(node: &RawNode, total: u64, depth: usize, budget: &mut usize) -> NodeDto {
    if !node.is_dir {
        *budget = budget.saturating_sub(1);
        return NodeDto {
            name: node.name.clone(),
            path: node.path.clone(),
            size: node.size,
            files: 1,
            dirs: 0,
            kind: "file",
            children: Vec::new(),
        };
    }

    // Collapse `a/b/c` chains: a folder holding a single folder and no file of
    // its own is just a longer name — drawing it as extra rings is noise.
    let mut current = node;
    let mut name = node.name.clone();
    while depth > 0
        && current.direct_files == 0
        && current.children.len() == 1
        && current.children[0].is_dir
        && depth < MAX_DEPTH
    {
        current = &current.children[0];
        name.push('/');
        name.push_str(&current.name);
    }

    let mut kids: Vec<&RawNode> = current.children.iter().collect();
    kids.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    let abs_floor = ((total as f64) * ABS_FLOOR).max(1.0) as u64;
    let rel_floor = ((current.size as f64) * REL_FLOOR) as u64;

    let mut kept: Vec<NodeDto> = Vec::new();

    for (index, child) in kids.iter().enumerate() {
        let wanted = depth < MAX_DEPTH
            && *budget > 0
            && kept.len() < MAX_CHILDREN
            && (index < MIN_TOP || child.size >= abs_floor.max(rel_floor));
        if wanted {
            kept.push(convert(child, total, depth + 1, budget));
        }
    }

    *budget = budget.saturating_sub(1);

    // Everything without an arc of its own — children below the floors, children
    // over MAX_CHILDREN, and loose files under FILE_FLOOR — still has to be
    // visible, otherwise the arcs would not add up to the folder in the middle.
    let kept_size: u64 = kept.iter().map(|child| child.size).sum();
    let kept_files: u64 = kept.iter().map(|child| child.files).sum();
    let kept_dirs: u64 = kept.iter().map(|child| child.dirs).sum();
    let rest_size = current.size.saturating_sub(kept_size);

    if rest_size > 0 {
        let rest_children = current.children.len().saturating_sub(kept.len());
        kept.push(NodeDto {
            name: if rest_children > 0 {
                format!("{} more", rest_children)
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

    kept.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    NodeDto {
        name,
        path: current.path.clone(),
        size: current.size,
        files: current.files,
        dirs: current.dirs,
        kind: "dir",
        children: kept,
    }
}

/// `C:\` -> `\\?\C:\` so paths beyond MAX_PATH still work.
fn verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\?\") {
        return path.to_path_buf();
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        return PathBuf::from(format!(r"\\?\UNC\{}", rest));
    }
    PathBuf::from(format!(r"\\?\{}", text))
}

/// Inverse of [`verbatim`], for display and for the webview.
fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        text
    }
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

    #[test]
    fn sums_file_sizes_recursively() {
        let tree = TempTree::new("sums");
        tree.file("a.bin", 1024);
        tree.file("sub/b.bin", 2048);
        tree.file("sub/deep/c.bin", 4096);

        let raw = scan_tree(&tree.0);
        assert_eq!(raw.size, 7168);
        assert_eq!(raw.files, 3);
        assert_eq!(raw.dirs, 2);
    }

    #[test]
    fn large_files_are_not_counted_twice() {
        let tree = TempTree::new("double");
        tree.file("big.bin", 256 * 1024);
        tree.file("sub/huge.bin", 512 * 1024);
        tree.file("sub/tiny.bin", 32);

        let raw = scan_tree(&tree.0);
        assert_eq!(raw.size, 768 * 1024 + 32, "bytes must be counted once");
        assert_eq!(raw.files, 3, "files must be counted once");

        let dto = to_dto(&raw, raw.size);
        let sum: u64 = dto.children.iter().map(|child| child.size).sum();
        assert_eq!(sum, raw.size, "arcs must add up to the folder total");
    }

    #[test]
    fn missing_directory_is_an_error_not_a_panic() {
        let missing = std::env::temp_dir().join("disk-scanner-does-not-exist-xyz");
        let err = scan(&missing, &AtomicBool::new(false), &ScanCounters::new()).unwrap_err();
        assert!(err.contains("Cannot open"), "unexpected error: {err}");
    }

    #[test]
    fn dto_sorts_children_and_aggregates_the_tail() {
        let tree = TempTree::new("agg");
        tree.file("big/one.bin", 100_000);
        tree.file("mid/two.bin", 50_000);
        // Forty tiny folders: they must not each become an arc.
        for index in 0..40 {
            tree.file(&format!("tiny{index}/x.bin"), 16);
        }

        let raw = scan_tree(&tree.0);
        let total = raw.size;
        let dto = to_dto(&raw, total);

        assert_eq!(dto.kind, "dir");
        assert_eq!(dto.size, total);
        assert!(dto.children.len() < 40, "tail was not aggregated");
        let sizes: Vec<u64> = dto.children.iter().map(|child| child.size).collect();
        let mut sorted = sizes.clone();
        sorted.sort_by(|a, b| b.cmp(a));
        assert_eq!(sizes, sorted, "children must be sorted by size");
        assert!(dto.children.iter().any(|child| child.kind == "other"));
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

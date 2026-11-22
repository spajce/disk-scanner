//! A finished scan written to disk, so a window has something to draw before
//! the walker has read a single directory.
//!
//! A cold walk of a spinning disk runs for minutes, and the tree it produces is
//! a few megabytes. Saving the compacted tree and loading it at start-up turns
//! that wait into a paint; the walk then runs behind it and refreshes the
//! numbers in place.
//!
//! The format is written by hand rather than through serde: the payload is one
//! shape of node, the file has to be read back by the same program that wrote
//! it, and a version byte is easier to reason about than a schema change.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::scanner::child_path;
use crate::NodeDto;

/// `DSKC` — disk cache. A file that does not start with this is not ours.
const MAGIC: [u8; 4] = *b"DSKC";
/// Bumped whenever the payload changes shape; a mismatch is a cache miss.
const VERSION: u16 = 1;
/// Deepest chain the encoder will write or read. `to_dto` caps the tree far
/// below this, so only a corrupt file can reach it.
const MAX_DEPTH: usize = 64;
/// Longest name or suffix accepted from a file. Windows MAX_PATH is 32 767.
const MAX_TEXT: u32 = 65_536;
/// Largest node count accepted from a file, to bound a corrupt header.
const MAX_NODES: u32 = 20_000_000;

/// What a finished scan is worth remembering, apart from the tree itself.
pub struct ScanMeta {
    /// The folder or volume that was walked.
    pub root: String,
    /// When the walk finished, in wall-clock terms, so the window can say how
    /// old the numbers are.
    pub scanned_at: SystemTime,
    pub elapsed: f64,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub errors: u64,
}

impl ScanMeta {
    /// How long ago the scan ran. A clock that went backwards reads as "now".
    pub fn age(&self) -> Duration {
        SystemTime::now()
            .duration_since(self.scanned_at)
            .unwrap_or_default()
    }
}

/// A snapshot read back from disk.
pub struct Snapshot {
    pub meta: ScanMeta,
    /// The compacted tree the window draws.
    pub tree: NodeDto,
}

/// Where a root's snapshot lives.
///
/// The file name is a hash of the normalised root, because a UNC path or a
/// trailing separator would not make a valid one; the root is stored inside the
/// file and compared on load, so a hash collision cannot show the wrong drive.
pub fn path_for(dir: &Path, root: &str) -> PathBuf {
    dir.join(format!("{:016x}.dsk", hash(&normalise(root))))
}

/// How many roots keep a snapshot. A tree is a few megabytes, so the directory
/// is bounded rather than allowed to grow with every folder ever scanned.
const KEEP: usize = 4;
/// A snapshot older than this is not worth the bytes.
const MAX_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Write `meta` and `tree`, replacing any earlier snapshot for the same root.
///
/// The payload is built in memory and then swapped in by rename: an interrupted
/// write leaves the previous snapshot intact instead of a half tree that the
/// next start-up would try to draw.
pub fn save(dir: &Path, meta: &ScanMeta, tree: &NodeDto) -> io::Result<()> {
    let mut out = Vec::with_capacity(1 << 20);
    encode(&mut out, meta, tree);

    fs::create_dir_all(dir)?;
    let final_path = path_for(dir, &meta.root);
    let temp = final_path.with_extension("tmp");
    {
        let mut file = fs::File::create(&temp)?;
        file.write_all(&out)?;
        file.sync_all()?;
    }
    fs::rename(&temp, &final_path)?;
    prune(dir, &final_path);
    Ok(())
}

/// Drop all but the newest [`KEEP`] snapshots, and any older than [`MAX_AGE`].
///
/// `fresh` is the file just written, which is never removed: two roots saved
/// inside the same timestamp tick would otherwise sort unpredictably.
fn prune(dir: &Path, fresh: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = SystemTime::now();
    let mut kept: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension()? != "dsk" || path == fresh {
                return None;
            }
            let at = entry.metadata().ok()?.modified().ok()?;
            Some((at, path))
        })
        .collect();

    for (at, path) in &kept {
        let stale = now.duration_since(*at).map(|age| age > MAX_AGE).unwrap_or(false);
        if stale {
            let _ = fs::remove_file(path);
        }
    }
    kept.retain(|(at, _)| now.duration_since(*at).map(|age| age <= MAX_AGE).unwrap_or(true));

    if kept.len() + 1 <= KEEP {
        return;
    }
    // Oldest first, so the extra ones are the first to go.
    kept.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let extra = kept.len() + 1 - KEEP;
    for (_, path) in kept.into_iter().take(extra) {
        let _ = fs::remove_file(path);
    }
}

/// Read the snapshot for `root`, or `None` when there is not a usable one.
///
/// Anything unexpected — a missing file, an older format, a file for a
/// different root, a truncated payload — is a miss rather than an error: the
/// caller can always walk the volume again.
pub fn load(dir: &Path, root: &str) -> Option<Snapshot> {
    let bytes = fs::read(path_for(dir, root)).ok()?;
    let snapshot = decode(&bytes)?;
    if normalise(&snapshot.meta.root) != normalise(root) {
        return None;
    }
    Some(snapshot)
}

fn normalise(root: &str) -> String {
    root.trim_end_matches(['\\', '/']).to_ascii_lowercase()
}

/// FNV-1a: stable across runs and releases, which a `DefaultHasher` is not.
fn hash(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn from_millis(value: u64) -> SystemTime {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(value))
        .unwrap_or_else(SystemTime::now)
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    put_u32(out, text.len() as u32);
    out.extend_from_slice(text.as_bytes());
}

fn count_nodes(node: &NodeDto) -> u32 {
    1 + node.children.iter().map(count_nodes).sum::<u32>()
}

fn encode(out: &mut Vec<u8>, meta: &ScanMeta, tree: &NodeDto) {
    out.extend_from_slice(&MAGIC);
    put_u16(out, VERSION);
    put_u64(out, millis(meta.scanned_at));
    put_u64(out, meta.elapsed.to_bits());
    put_u64(out, meta.files);
    put_u64(out, meta.dirs);
    put_u64(out, meta.bytes);
    put_u64(out, meta.errors);
    put_str(out, &meta.root);
    put_u32(out, count_nodes(tree));
    encode_node(out, tree, "");
}

/// One node, pre-order, with `parent` supplied by the caller so the path can be
/// written as the part the parent does not already contain.
fn encode_node(out: &mut Vec<u8>, node: &NodeDto, parent: &str) {
    let kind = match node.kind {
        "dir" => 0u8,
        "file" => 1,
        _ => 2,
    };
    out.push(kind);
    put_u64(out, node.size);
    put_u64(out, node.files);
    put_u64(out, node.dirs);
    put_u32(out, node.children.len() as u32);
    put_str(out, &node.name);

    // Most paths are just the parent plus the name. A folded chain (`a/b/c`) is
    // the exception: its name is display text and its path uses separators, so
    // it carries the difference explicitly. An aggregated `(n smaller items)`
    // node has no path at all, and must not be given one on the way back.
    if node.path.is_empty() {
        out.push(2);
    } else if child_path(parent, &node.name) == node.path {
        out.push(0);
    } else {
        out.push(1);
        put_str(out, node.path.strip_prefix(parent).unwrap_or(&node.path));
    }

    for child in &node.children {
        encode_node(out, child, &node.path);
    }
}

fn decode(bytes: &[u8]) -> Option<Snapshot> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4)? != MAGIC {
        return None;
    }
    if reader.u16()? != VERSION {
        return None;
    }
    let scanned_at = from_millis(reader.u64()?);
    let elapsed = f64::from_bits(reader.u64()?);
    let files = reader.u64()?;
    let dirs = reader.u64()?;
    let bytes_walked = reader.u64()?;
    let errors = reader.u64()?;
    let root = reader.string()?;
    let declared = reader.u32()?;
    if declared > MAX_NODES {
        return None;
    }

    let tree = decode_node(&mut reader, "", 0)?;
    // The header count is only a sanity check: a payload that disagrees with it
    // was truncated or overwritten, and a partial tree would draw wrong sizes.
    if count_nodes(&tree) != declared {
        return None;
    }

    Some(Snapshot {
        meta: ScanMeta {
            root,
            scanned_at,
            elapsed,
            files,
            dirs,
            bytes: bytes_walked,
            errors,
        },
        tree,
    })
}

fn decode_node(reader: &mut Reader<'_>, parent: &str, depth: usize) -> Option<NodeDto> {
    if depth > MAX_DEPTH {
        return None;
    }
    let kind = match reader.u8()? {
        0 => "dir",
        1 => "file",
        2 => "other",
        _ => return None,
    };
    let size = reader.u64()?;
    let files = reader.u64()?;
    let dirs = reader.u64()?;
    let child_count = reader.u32()?;
    if child_count > MAX_NODES {
        return None;
    }
    let name = reader.string()?;
    let path = match reader.u8()? {
        0 => child_path(parent, &name),
        1 => {
            let suffix = reader.string()?;
            format!("{parent}{suffix}")
        }
        2 => String::new(),
        _ => return None,
    };

    // Reserve a sane amount up front: `child_count` comes from the file, so it
    // is grown by pushing rather than trusted for one big allocation.
    let mut children = Vec::with_capacity(child_count.min(1024) as usize);
    for _ in 0..child_count {
        children.push(decode_node(reader, &path, depth + 1)?);
    }

    Some(NodeDto {
        name,
        path,
        size,
        files,
        dirs,
        kind,
        children,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(count)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self) -> Option<String> {
        let length = self.u32()?;
        if length > MAX_TEXT {
            return None;
        }
        Some(String::from_utf8(self.take(length as usize)?.to_vec()).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, path: &str, size: u64, kind: &'static str) -> NodeDto {
        NodeDto {
            name: name.to_string(),
            path: path.to_string(),
            size,
            files: 1,
            dirs: 0,
            kind,
            children: Vec::new(),
        }
    }

    fn sample() -> NodeDto {
        let mut root = node("C:\\", "C:\\", 4096, "dir");
        let mut users = node("Users", "C:\\Users", 3072, "dir");
        // A folded chain: the name is display text, the path uses separators.
        let mut folded = node("jcarillo/AppData/Local", "C:\\Users\\jcarillo\\AppData\\Local", 2048, "dir");
        folded.children.push(node("Cache", "C:\\Users\\jcarillo\\AppData\\Local\\Cache", 2048, "dir"));
        users.children.push(folded);
        users.children.push(node("note.txt", "C:\\Users\\note.txt", 1024, "file"));
        root.children.push(users);
        root.children.push(node("smaller files", "", 1024, "other"));
        root
    }

    fn meta() -> ScanMeta {
        ScanMeta {
            root: "C:\\".to_string(),
            scanned_at: UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            elapsed: 6.5,
            files: 3,
            dirs: 5,
            bytes: 4096,
            errors: 2,
        }
    }

    fn assert_same(expected: &NodeDto, actual: &NodeDto) {
        assert_eq!(expected.name, actual.name);
        assert_eq!(expected.path, actual.path, "path of {}", expected.name);
        assert_eq!(expected.size, actual.size);
        assert_eq!(expected.files, actual.files);
        assert_eq!(expected.dirs, actual.dirs);
        assert_eq!(expected.kind, actual.kind);
        assert_eq!(expected.children.len(), actual.children.len());
        for (left, right) in expected.children.iter().zip(&actual.children) {
            assert_same(left, right);
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "disk-scanner-cache-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_snapshot_round_trips() {
        let dir = temp_dir("round-trip");
        let written = meta();
        save(&dir, &written, &sample()).unwrap();

        let read = load(&dir, "C:\\").expect("the snapshot just written is there");
        assert_eq!(read.meta.root, written.root);
        assert_eq!(read.meta.files, written.files);
        assert_eq!(read.meta.dirs, written.dirs);
        assert_eq!(read.meta.bytes, written.bytes);
        assert_eq!(read.meta.errors, written.errors);
        assert_eq!(read.meta.elapsed, written.elapsed);
        assert_eq!(millis(read.meta.scanned_at), millis(written.scanned_at));
        assert_same(&sample(), &read.tree);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folded_chain_keeps_its_path_and_its_label() {
        let dir = temp_dir("folded");
        save(&dir, &meta(), &sample()).unwrap();
        let read = load(&dir, "c:\\").expect("the root is compared case-insensitively");

        let folded = &read.tree.children[0].children[0];
        assert_eq!(folded.name, "jcarillo/AppData/Local", "the label is not a path");
        assert_eq!(
            folded.path, "C:\\Users\\jcarillo\\AppData\\Local",
            "the path uses separators, not the slash-joined label"
        );
        assert_eq!(
            folded.children[0].path,
            "C:\\Users\\jcarillo\\AppData\\Local\\Cache"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_file_is_a_miss_not_a_panic() {
        let dir = temp_dir("truncated");
        save(&dir, &meta(), &sample()).unwrap();
        let path = path_for(&dir, "C:\\");
        let bytes = fs::read(&path).unwrap();

        for cut in [0, 4, 12, bytes.len() / 2, bytes.len() - 1] {
            fs::write(&path, &bytes[..cut]).unwrap();
            assert!(
                load(&dir, "C:\\").is_none(),
                "a file cut to {cut} bytes must not load"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_roots_snapshot_is_not_used() {
        let dir = temp_dir("other-root");
        save(&dir, &meta(), &sample()).unwrap();
        assert!(load(&dir, "D:\\").is_none(), "D: has no snapshot");
        assert!(load(&dir, "C:\\Users").is_none(), "a subfolder is a different root");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_foreign_file_is_a_miss() {
        let dir = temp_dir("foreign");
        let path = path_for(&dir, "C:\\");
        fs::write(&path, b"not a disk scanner snapshot").unwrap();
        assert!(load(&dir, "C:\\").is_none());

        // Right magic, wrong version: a file from an older release.
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&(VERSION + 1).to_le_bytes());
        bytes.extend_from_slice(&[0u8; 64]);
        fs::write(&path, &bytes).unwrap();
        assert!(load(&dir, "C:\\").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_few_roots_keep_a_snapshot() {
        let dir = temp_dir("prune");
        for index in 0..(KEEP + 3) {
            let mut meta = meta();
            meta.root = format!("{}:\\", (b'A' + index as u8) as char);
            save(&dir, &meta, &sample()).unwrap();
        }

        let files = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "dsk"))
            .count();
        assert_eq!(files, KEEP, "the directory is bounded, not accumulating");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_path_is_named_after_the_normalised_root() {
        let dir = Path::new("cache");
        assert_eq!(path_for(dir, "C:\\"), path_for(dir, "c:"));
        assert_eq!(path_for(dir, "\\\\server\\share\\"), path_for(dir, "\\\\SERVER\\share"));
        assert_ne!(path_for(dir, "C:\\"), path_for(dir, "D:\\"));
    }
}

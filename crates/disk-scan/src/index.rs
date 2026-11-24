//! Whole-scan file index.
//!
//! The compacted tree keeps folders and the handful of files that are drawn as
//! arcs. The app also needs the files the tree throws away, so the walker feeds
//! this index as it goes: no second pass, no extra `stat`.
//!
//! Two lists come out of it:
//!
//! * `files` — the [`TOP_FILES`] biggest files anywhere under the root, for the
//!   "Largest files" view;
//! * `big` — the [`BIG_FILES`] biggest files at or above [`BIG_FLOOR`], the
//!   candidate set for duplicate detection.
//!
//! Together with a per-category byte/file count over *every* file, that is
//! enough for a type breakdown without walking the tree again.
//!
//! A file only gets a `FileHit` (and therefore an allocated path) when it beats
//! a list's floor. The floors start at zero and rise as lists fill, so a volume
//! with millions of files still pays a path copy only for the few that can
//! matter.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::scanner::display_path;

/// How many of the biggest files are kept for the file list.
pub const TOP_FILES: usize = 20_000;
/// How many large files are kept as duplicate candidates.
pub const BIG_FILES: usize = 60_000;
/// Files at or above this size are considered for duplicate detection.
pub const BIG_FLOOR: u64 = 1024 * 1024;

/// File kinds the breakdown groups by. The order is the order the view lists
/// them in before it sorts by size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Documents,
    Images,
    Video,
    Audio,
    Archives,
    Installers,
    Code,
    DiskImages,
    Backups,
    Fonts,
    Other,
}

impl Category {
    pub const ALL: [Category; 11] = [
        Category::Documents,
        Category::Images,
        Category::Video,
        Category::Audio,
        Category::Archives,
        Category::Installers,
        Category::Code,
        Category::DiskImages,
        Category::Backups,
        Category::Fonts,
        Category::Other,
    ];

    /// Title shown in the breakdown.
    pub fn label(self) -> &'static str {
        match self {
            Category::Documents => "Documents",
            Category::Images => "Images",
            Category::Video => "Video",
            Category::Audio => "Audio",
            Category::Archives => "Archives",
            Category::Installers => "Installers",
            Category::Code => "Code",
            Category::DiskImages => "Disk images",
            Category::Backups => "Backups",
            Category::Fonts => "Fonts",
            Category::Other => "Other",
        }
    }

    /// Stable identifier for the CSV export and the cached snapshot.
    pub fn slug(self) -> &'static str {
        match self {
            Category::Documents => "documents",
            Category::Images => "images",
            Category::Video => "video",
            Category::Audio => "audio",
            Category::Archives => "archives",
            Category::Installers => "installers",
            Category::Code => "code",
            Category::DiskImages => "disk-images",
            Category::Backups => "backups",
            Category::Fonts => "fonts",
            Category::Other => "other",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Category::Documents => 0,
            Category::Images => 1,
            Category::Video => 2,
            Category::Audio => 3,
            Category::Archives => 4,
            Category::Installers => 5,
            Category::Code => 6,
            Category::DiskImages => 7,
            Category::Backups => 8,
            Category::Fonts => 9,
            Category::Other => 10,
        }
    }

    pub fn from_index(index: usize) -> Category {
        Category::ALL[index.min(Category::ALL.len() - 1)]
    }

    /// The kind of a file by its name, using the text after the last dot.
    pub fn of_name(name: &str) -> Category {
        Category::of_bytes(name.as_bytes())
    }

    /// The kind of a path by its final component.
    pub fn of_path(path: &Path) -> Category {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            match path.file_name() {
                Some(name) => Category::of_bytes(name.as_bytes()),
                None => Category::Other,
            }
        }
        #[cfg(not(unix))]
        {
            match path.file_name().and_then(|name| name.to_str()) {
                Some(name) => Category::of_name(name),
                None => Category::Other,
            }
        }
    }

    /// The kind of a full file name. A leading dot (`.bashrc`) is a name, not
    /// an extension.
    pub fn of_bytes(name: &[u8]) -> Category {
        let Some(dot) = name.iter().rposition(|&byte| byte == b'.') else {
            return Category::Other;
        };
        if dot == 0 || dot + 1 >= name.len() || name.len() - dot - 1 > 16 {
            return Category::Other;
        }
        let mut ext = [0u8; 16];
        for (slot, &byte) in ext.iter_mut().zip(&name[dot + 1..]) {
            *slot = byte.to_ascii_lowercase();
        }
        Category::of_ascii(&ext[..name.len() - dot - 1])
    }

    /// The kind of a UTF-16 file name, for the Windows listing's wide buffer.
    pub fn of_wide(name: &[u16]) -> Category {
        let Some(dot) = name.iter().rposition(|&unit| unit == b'.' as u16) else {
            return Category::Other;
        };
        let ext = &name[dot + 1..];
        if dot == 0 || ext.is_empty() || ext.len() > 16 {
            return Category::Other;
        }
        let mut ascii = [0u8; 16];
        for (slot, &unit) in ascii.iter_mut().zip(ext) {
            if unit > 0x7f {
                return Category::Other;
            }
            *slot = (unit as u8).to_ascii_lowercase();
        }
        Category::of_ascii(&ascii[..ext.len()])
    }

    fn of_ascii(ext: &[u8]) -> Category {
        match ext {
            b"pdf" | b"doc" | b"docx" | b"xls" | b"xlsx" | b"ppt" | b"pptx" | b"txt" | b"rtf"
            | b"odt" | b"ods" | b"odp" | b"csv" | b"epub" | b"mobi" | b"azw3" | b"md"
            | b"pages" | b"numbers" | b"key" => Category::Documents,
            b"jpg" | b"jpeg" | b"jfif" | b"png" | b"gif" | b"bmp" | b"tif" | b"tiff"
            | b"webp" | b"heic" | b"heif" | b"avif" | b"svg" | b"ico" | b"raw" | b"cr2"
            | b"cr3" | b"nef" | b"arw" | b"dng" | b"orf" | b"rw2" | b"psd" | b"xcf" | b"ai"
            | b"eps" => Category::Images,
            // `.ts` is claimed by both TypeScript and MPEG transport streams.
            // The stream is the one big enough to move a total, so video wins.
            b"mp4" | b"m4v" | b"mkv" | b"avi" | b"mov" | b"wmv" | b"flv" | b"webm" | b"mpg"
            | b"mpeg" | b"m2ts" | b"mts" | b"ts" | b"3gp" | b"ogv" | b"vob" => Category::Video,
            b"mp3" | b"wav" | b"flac" | b"aac" | b"ogg" | b"oga" | b"m4a" | b"wma" | b"aiff"
            | b"aif" | b"opus" | b"mid" | b"midi" | b"amr" | b"ape" => Category::Audio,
            b"zip" | b"7z" | b"rar" | b"tar" | b"gz" | b"tgz" | b"bz2" | b"tbz2" | b"xz"
            | b"zst" | b"cab" | b"lzh" | b"arj" => Category::Archives,
            b"exe" | b"msi" | b"msix" | b"appx" | b"appxbundle" | b"dmg" | b"pkg" | b"deb"
            | b"rpm" | b"appimage" | b"apk" => Category::Installers,
            b"rs" | b"tsx" | b"js" | b"jsx" | b"mjs" | b"cjs" | b"py" | b"java"
            | b"c" | b"h" | b"cpp" | b"cc" | b"cxx" | b"hpp" | b"cs" | b"go" | b"rb"
            | b"php" | b"swift" | b"kt" | b"kts" | b"scala" | b"lua" | b"pl" | b"pm"
            | b"sh" | b"bash" | b"zsh" | b"bat" | b"cmd" | b"ps1" | b"html" | b"htm"
            | b"css" | b"scss" | b"sass" | b"less" | b"json" | b"toml" | b"yaml" | b"yml"
            | b"xml" | b"sql" | b"vue" | b"svelte" | b"dart" | b"ipynb" | b"gradle"
            | b"cmake" => Category::Code,
            b"iso" | b"img" | b"vhd" | b"vhdx" | b"vmdk" | b"vdi" | b"qcow" | b"qcow2"
            | b"wim" | b"vcd" => Category::DiskImages,
            b"bak" | b"backup" | b"bkp" | b"old" | b"orig" | b"tmp" | b"temp" | b"swp"
            | b"swo" | b"sav" | b"wbcat" | b"bkf" => Category::Backups,
            b"ttf" | b"otf" | b"woff" | b"woff2" | b"ttc" | b"fon" | b"pfb" | b"pfm" => {
                Category::Fonts
            }
            _ => Category::Other,
        }
    }
}

/// Bytes and files of one category.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bucket {
    pub bytes: u64,
    pub files: u64,
}

/// Per-category totals over every file the walk counted.
#[derive(Clone, Debug, Default)]
pub struct Buckets {
    counts: [Bucket; Category::ALL.len()],
}

impl Buckets {
    pub fn add(&mut self, category: Category, size: u64) {
        let slot = &mut self.counts[category.index()];
        slot.bytes = slot.bytes.saturating_add(size);
        slot.files = slot.files.saturating_add(1);
    }

    pub fn merge(&mut self, other: &Buckets) {
        for (slot, add) in self.counts.iter_mut().zip(&other.counts) {
            slot.bytes = slot.bytes.saturating_add(add.bytes);
            slot.files = slot.files.saturating_add(add.files);
        }
    }

    pub fn get(&self, category: Category) -> Bucket {
        self.counts[category.index()]
    }

    pub fn total(&self) -> Bucket {
        self.counts.iter().fold(Bucket::default(), |sum, slot| Bucket {
            bytes: sum.bytes.saturating_add(slot.bytes),
            files: sum.files.saturating_add(slot.files),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.counts.iter().all(|slot| slot.files == 0)
    }

    pub fn iter(&self) -> impl Iterator<Item = (Category, Bucket)> + '_ {
        Category::ALL
            .into_iter()
            .map(|category| (category, self.counts[category.index()]))
    }

    /// Mutable access to one category's totals, for the snapshot decoder.
    pub(crate) fn slot(&mut self, category: Category) -> &mut Bucket {
        &mut self.counts[category.index()]
    }
}

/// One file the index kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHit {
    /// Display path, the same shape the tree uses (`\\?\` stripped).
    pub path: String,
    pub size: u64,
    /// Seconds since the Unix epoch, `-1` when the walk could not tell.
    pub modified: i64,
}

/// What a finished walk knows about files outside the tree.
#[derive(Clone, Debug, Default)]
pub struct ScanIndex {
    /// Biggest files first, at most [`TOP_FILES`].
    pub files: Vec<FileHit>,
    /// Large files first, at most [`BIG_FILES`]; duplicate candidates.
    pub big: Vec<FileHit>,
    pub buckets: Buckets,
}

impl ScanIndex {
    /// Bytes and files across every category.
    pub fn total(&self) -> Bucket {
        self.buckets.total()
    }
}

/// Collects [`ScanIndex`] while the walker runs.
pub struct FileIndex {
    top: Mutex<Vec<FileHit>>,
    top_floor: AtomicU64,
    top_cap: usize,
    big: Mutex<Vec<FileHit>>,
    big_floor: AtomicU64,
    big_cap: usize,
    buckets: Mutex<Buckets>,
}

impl Default for FileIndex {
    fn default() -> Self {
        Self::new()
    }
}

impl FileIndex {
    pub fn new() -> Self {
        FileIndex::with_caps(TOP_FILES, BIG_FILES)
    }

    fn with_caps(top_cap: usize, big_cap: usize) -> Self {
        FileIndex {
            top: Mutex::new(Vec::new()),
            top_floor: AtomicU64::new(0),
            top_cap,
            big: Mutex::new(Vec::new()),
            // A file needs to be at least `BIG_FLOOR` bytes to be listed.
            big_floor: AtomicU64::new(BIG_FLOOR - 1),
            big_cap,
            buckets: Mutex::new(Buckets::default()),
        }
    }

    /// Whether [`FileIndex::offer`] would keep this size. Lets a caller decide
    /// whether building a path for it is worth the allocation.
    pub fn wants(&self, size: u64) -> bool {
        size > self.top_floor.load(Ordering::Relaxed)
            || (size >= BIG_FLOOR && size > self.big_floor.load(Ordering::Relaxed))
    }

    /// Offer one walked file. Cheap when the file is below both floors.
    pub fn offer(&self, path: &Path, size: u64, modified: i64) {
        if size == 0 {
            return;
        }
        let keep_top = size > self.top_floor.load(Ordering::Relaxed);
        let keep_big = size >= BIG_FLOOR && size > self.big_floor.load(Ordering::Relaxed);
        if !keep_top && !keep_big {
            return;
        }
        let text = display_path(path);
        if keep_top {
            Self::push(&self.top, &self.top_floor, self.top_cap, FileHit {
                path: text.clone(),
                size,
                modified,
            });
        }
        if keep_big {
            Self::push(&self.big, &self.big_floor, self.big_cap, FileHit {
                path: text,
                size,
                modified,
            });
        }
    }

    fn push(list: &Mutex<Vec<FileHit>>, floor: &AtomicU64, cap: usize, hit: FileHit) {
        let mut guard = match list.lock() {
            Ok(guard) => guard,
            // A poisoned lock means a worker panicked; the list is still usable
            // as a bag of candidates, so keep going rather than dropping it.
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.push(hit);
        // Trim only once the buffer is twice the cap: sorting on every push
        // would turn the hot path into a comparison storm.
        if guard.len() > cap.max(1) * 2 {
            guard.sort_unstable_by(|left, right| right.size.cmp(&left.size));
            guard.truncate(cap.max(1));
            if let Some(last) = guard.last() {
                floor.store(last.size, Ordering::Relaxed);
            }
        }
    }

    /// Merge the totals of one directory listing.
    pub fn add_buckets(&self, buckets: &Buckets) {
        match self.buckets.lock() {
            Ok(mut guard) => guard.merge(buckets),
            Err(poisoned) => poisoned.into_inner().merge(buckets),
        }
    }

    /// The finished index, biggest files first.
    pub fn finish(self) -> ScanIndex {
        let mut files = self
            .top
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        files.sort_unstable_by(|left, right| {
            right
                .size
                .cmp(&left.size)
                .then_with(|| left.path.cmp(&right.path))
        });
        files.truncate(self.top_cap.max(1));

        let mut big = self
            .big
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        big.sort_unstable_by(|left, right| {
            right
                .size
                .cmp(&left.size)
                .then_with(|| left.path.cmp(&right.path))
        });
        big.truncate(self.big_cap.max(1));

        let buckets = self
            .buckets
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        ScanIndex {
            files,
            big,
            buckets,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_extensions() {
        let cases = [
            ("movie.mp4", Category::Video),
            ("photo.JPEG", Category::Images),
            ("song.flac", Category::Audio),
            ("report.pdf", Category::Documents),
            ("backup.zip", Category::Archives),
            ("setup.msi", Category::Installers),
            ("main.rs", Category::Code),
            ("debian.iso", Category::DiskImages),
            (".bashrc", Category::Other),
            ("notes", Category::Other),
            ("archive.unknownext", Category::Other),
        ];
        for (name, expected) in cases {
            assert_eq!(Category::of_name(name), expected, "{name}");
        }
    }

    #[test]
    fn every_category_has_a_distinct_slot() {
        let mut seen = [false; Category::ALL.len()];
        for category in Category::ALL {
            assert!(!seen[category.index()], "{category:?} shares a slot");
            seen[category.index()] = true;
            assert_eq!(Category::from_index(category.index()), category);
            assert!(!category.label().is_empty());
            assert!(!category.slug().is_empty());
        }
    }

    #[test]
    fn buckets_sum_and_merge() {
        let mut left = Buckets::default();
        left.add(Category::Video, 100);
        left.add(Category::Video, 50);
        left.add(Category::Images, 25);
        let mut right = Buckets::default();
        right.add(Category::Video, 5);
        left.merge(&right);

        assert_eq!(left.get(Category::Video), Bucket { bytes: 155, files: 3 });
        assert_eq!(left.get(Category::Images), Bucket { bytes: 25, files: 1 });
        assert_eq!(left.total(), Bucket { bytes: 180, files: 4 });
        assert!(!left.is_empty());
        assert_eq!(left.iter().count(), Category::ALL.len());
    }

    #[test]
    fn keeps_only_the_biggest_files() {
        let index = FileIndex::with_caps(3, 3);
        for size in (1..=10).rev() {
            index.offer(Path::new("C:\\x\\file.bin"), size, 0);
        }
        // The list trims itself when it overflows, so the first six offers are
        // enough to raise the floor above the small ones.
        let scan = index.finish();
        assert!(scan.files.len() <= 3);
        let sizes: Vec<u64> = scan.files.iter().map(|hit| hit.size).collect();
        assert_eq!(sizes, vec![10, 9, 8]);
    }

    #[test]
    fn a_floor_rises_as_the_list_fills() {
        let index = FileIndex::with_caps(2, 2);
        for size in [1_000_000u64, 2, 3, 4, 5, 6] {
            index.offer(Path::new("C:\\x\\file.bin"), size, 0);
        }
        // After the buffer trims, only files bigger than the kept ones are
        // accepted, so a tiny file offered later is dropped without a path.
        assert!(!index.wants(1));
        assert!(index.wants(1_000_001));
    }

    #[test]
    fn big_files_need_a_megabyte() {
        let index = FileIndex::with_caps(100, 100);
        index.offer(Path::new("C:\\x\\small.bin"), BIG_FLOOR - 1, 0);
        index.offer(Path::new("C:\\x\\large.bin"), BIG_FLOOR, 0);
        let scan = index.finish();
        assert_eq!(scan.files.len(), 2, "both belong in the largest-file list");
        assert_eq!(scan.big.len(), 1, "only the big one is a duplicate candidate");
        assert!(scan.big[0].path.ends_with("large.bin"));
    }

    #[test]
    fn buckets_only_count_offered_directories() {
        let index = FileIndex::new();
        let mut buckets = Buckets::default();
        buckets.add(Category::Documents, 10);
        index.add_buckets(&buckets);
        index.add_buckets(&Buckets::default());
        assert_eq!(index.finish().total(), Bucket { bytes: 10, files: 1 });
    }
}

//! Round-trip check for the start-up snapshot.
//!
//! Walks a root, saves the compacted tree the way the window does after a scan,
//! reads it back and compares the two trees. The timings and the file size are
//! what a start-up pays before it can draw.
//!
//! `cargo run --release --example snapshot -- C:\`
//! `cargo run --release --example snapshot -- D:\ <some\empty\dir>`

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Instant, SystemTime};

use disk_scan::cache::{self, ScanMeta};
use disk_scan::{scan_indexed, to_dto, NodeDto, ScanCounters};

fn count(node: &NodeDto) -> usize {
    1 + node.children.iter().map(count).sum::<usize>()
}

/// Compare what a start-up would draw with what the walk produced.
fn same(expected: &NodeDto, actual: &NodeDto) -> Result<(), String> {
    if expected.name != actual.name
        || expected.path != actual.path
        || expected.size != actual.size
        || expected.files != actual.files
        || expected.dirs != actual.dirs
        || expected.kind != actual.kind
        || expected.children.len() != actual.children.len()
    {
        return Err(format!(
            "{}: {} ({}) vs {} ({})",
            expected.name, expected.size, expected.files, actual.size, actual.files
        ));
    }
    for (left, right) in expected.children.iter().zip(&actual.children) {
        same(left, right)?;
    }
    Ok(())
}

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| String::from("C:\\"));
    let dir = match std::env::args().nth(2) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::temp_dir().join("disk-scanner-snapshot-example"),
    };

    let counters = ScanCounters::new();
    let walk = Instant::now();
    let raw = match scan_indexed(Path::new(&target), &AtomicBool::new(false), &counters) {
        Ok((raw, index)) => (raw, index),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let (raw, index) = raw;
    let walk = walk.elapsed();
    let dto = to_dto(&raw, raw.size);
    let nodes = count(&dto);

    let meta = ScanMeta {
        root: target.clone(),
        scanned_at: SystemTime::now(),
        elapsed: walk.as_secs_f64(),
        files: counters.files.load(Ordering::Relaxed),
        dirs: counters.dirs.load(Ordering::Relaxed),
        bytes: counters.bytes.load(Ordering::Relaxed),
        errors: counters.errors.load(Ordering::Relaxed),
    };

    let saving = Instant::now();
    if let Err(error) = cache::save(&dir, &meta, &dto, &index) {
        eprintln!("save failed: {error}");
        std::process::exit(1);
    }
    let saved = saving.elapsed();

    let file = cache::path_for(&dir, &target);
    let bytes = std::fs::metadata(&file).map(|meta| meta.len()).unwrap_or(0);
    let per_node = if nodes > 0 {
        bytes as f64 / nodes as f64
    } else {
        0.0
    };

    let loading = Instant::now();
    let loaded = match cache::load(&dir, &target) {
        Some(snapshot) => snapshot,
        None => {
            eprintln!("load failed: {}", file.display());
            std::process::exit(1);
        }
    };
    let loaded_in = loading.elapsed();

    println!("walk {walk:?} · {nodes} nodes");
    println!(
        "save {saved:?} · {} ({:.1} MB, {per_node:.1} B/node)",
        file.display(),
        bytes as f64 / (1024.0 * 1024.0)
    );
    println!(
        "load {loaded_in:?} · {} nodes · {} files · {} dirs",
        count(&loaded.tree),
        loaded.meta.files,
        loaded.meta.dirs
    );

    match same(&dto, &loaded.tree) {
        Ok(()) => println!("trees match"),
        Err(difference) => {
            eprintln!("trees differ at {difference}");
            std::process::exit(1);
        }
    }
}

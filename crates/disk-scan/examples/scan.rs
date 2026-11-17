//! Ad-hoc benchmark: `cargo run --release --example scan -- C:\`

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use disk_scan::{scan, to_dto, ScanCounters};


fn count(node: &disk_scan::NodeDto) -> usize {
    1 + node.children.iter().map(count).sum::<usize>()
}

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| String::from("C:\\"));
    let cancel = AtomicBool::new(false);
    let counters = ScanCounters::new();

    let started = Instant::now();
    let root = match scan(Path::new(&target), &cancel, &counters) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed();

    let compacted = Instant::now();
    let dto = to_dto(&root, root.size);
    let compacted = compacted.elapsed();

    println!(
        "walk {elapsed:?} · compact {compacted:?} · {} files · {} dirs · {} bytes · {} nodes · {} errors",
        counters.files.load(Ordering::Relaxed),
        counters.dirs.load(Ordering::Relaxed),
        counters.bytes.load(Ordering::Relaxed),
        count(&dto),
        counters.errors.load(Ordering::Relaxed),
    );
}

//! Ad-hoc benchmark: `cargo run --release --example scan -- C:\`

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use disk_scan::{scan, to_dto, walk_threads, ScanCounters};


fn count(node: &disk_scan::NodeDto) -> usize {
    1 + node.children.iter().map(count).sum::<usize>()
}

/// Reports the walk's own counters every couple of seconds.
///
/// A cold walk of a spinning disk runs for minutes with no output, which reads
/// as a hang; the counters say it is alive and what it is reading. Silent for
/// the first few seconds, so a fast scan prints nothing extra.
fn follow(counters: Arc<ScanCounters>, done: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(3));
            if done.load(Ordering::Relaxed) {
                break;
            }
            let (files, dirs, bytes, errors, path) = counters.snapshot();
            eprintln!("  {files} files · {dirs} dirs · {bytes} bytes · {errors} errors · {path}");
        }
    });
}

fn main() {
    let target = std::env::args().nth(1).unwrap_or_else(|| String::from("C:\\"));
    let cancel = AtomicBool::new(false);
    let counters = Arc::new(ScanCounters::new());
    let done = Arc::new(AtomicBool::new(false));
    follow(Arc::clone(&counters), Arc::clone(&done));

    let started = Instant::now();
    let forced = std::env::var("DISK_SCAN_THREADS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok());
    let policy = match (walk_threads(Path::new(&target)), forced) {
        (Some(count), Some(_)) => format!("{count} threads (DISK_SCAN_THREADS)"),
        (Some(count), None) => format!("{count} threads (seek-penalty volume)"),
        (None, Some(_)) => String::from("the global pool (DISK_SCAN_THREADS=0)"),
        (None, None) => String::from("the global pool"),
    };
    eprintln!("walking {target} on {policy}");
    let root = match scan(Path::new(&target), &cancel, &counters) {
        Ok(root) => root,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    let elapsed = started.elapsed();
    done.store(true, Ordering::Relaxed);

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

    // `--children <path>` prints one level of the compacted payload, which is
    // how the "(n smaller items)" buckets are diagnosed.
    if let Some(path) = std::env::args().nth(2) {
        match find(&dto, &path) {
            Some(node) => {
                println!("\n{} · {} · {} children", node.name, node.size, node.children.len());
                for child in &node.children {
                    println!("  {:>14}  {:<6} {}", child.size, child.kind, child.name);
                }
            }
            None => println!("not found: {path}"),
        }
    }
}

fn find<'a>(node: &'a disk_scan::NodeDto, path: &str) -> Option<&'a disk_scan::NodeDto> {
    if node.path.eq_ignore_ascii_case(path) {
        return Some(node);
    }
    node.children.iter().find_map(|child| find(child, path))
}

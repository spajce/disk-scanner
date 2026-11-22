//! Disk usage scanning: a parallel directory walker, its compacted node tree,
//! and volume discovery. No UI toolkits in here so both desktop front ends can
//! share it.

pub mod cache;
pub mod drives;
pub mod scanner;

pub use cache::{ScanMeta, Snapshot};
pub use drives::{list as list_drives, DriveInfo};
pub use scanner::{
    scan, scan_pausable, to_dto, walk_threads, NodeDto, RawNode, ScanCounters,
};

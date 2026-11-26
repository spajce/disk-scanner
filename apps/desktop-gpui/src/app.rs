//! The disk-scanner window: a sunburst you can zoom, plus the file and type
//! breakdowns that answer "what is actually taking the room".

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::prelude::*;
use gpui::{
    canvas, div, linear_color_stop, linear_gradient, point, px, quad, relative, size,
    uniform_list, AnyElement, App, BorderStyle, Bounds, CursorStyle,
    Context, Div, FocusHandle, FontWeight, Hsla, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, Render, ScrollHandle,
    SharedString, UniformListScrollHandle, Window, WindowOptions,
};

use disk_scan::{
    find_duplicates, list_drives, scan_pausable_indexed, to_dto, Category, DriveInfo, DupGroup,
    DuplicateScan, FileHit, ScanCounters, ScanIndex, ScanMeta,
};

use crate::model::{
    build_chart, heatmap, hit, log_rank, short_name, treemap, ChartModel, Geom, Kind, Metric, Node,
    Tile, View, MAX_RINGS, TREEMAP_HEADER,
};
use crate::theme;
use crate::update;

const TOPBAR_H: f32 = 62.0;
const STATUSBAR_H: f32 = 30.0;
const PANEL_W: f32 = 352.0;
const GAP: f32 = 14.0;
const PAD: f32 = 14.0;

// The hover tooltip is an overlay card: fixed width, and tall enough for the
// name, the path and the numbers line.
const TOOLTIP_W: f32 = 280.0;
const TOOLTIP_H: f32 = 84.0;

struct ScanJob {
    cancel: Arc<AtomicBool>,
    /// Set while the walk is parked; cleared by Continue.
    pause: Arc<AtomicBool>,
    generation: u64,
    /// Wall time spent parked, kept out of the reported scan duration.
    paused: Duration,
    paused_since: Option<Instant>,
}

impl ScanJob {
    /// Abandon the walk: cancel it and release it if it is parked on `pause`.
    fn abort(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        self.pause.store(false, Ordering::Relaxed);
    }
}

enum Phase {
    Empty,
    Scanning,
    /// The walk is parked and keeps its counters; Continue resumes it.
    Stopped,
    Ready {
        elapsed: f64,
        files: u64,
        bytes: u64,
    },
    Failed(String),
}

struct Tooltip {
    x: f32,
    y: f32,
    index: usize,
}

#[derive(Clone)]
struct Menu {
    x: f32,
    y: f32,
    node: Arc<Node>,
}

// The context menu is painted by the chart canvas and hit-tested from the
// pointer position, because GPUI's popup layers paint at the right place but
// never receive presses on this backend.
const MENU_W: f32 = 232.0;
const MENU_PAD: f32 = 5.0;
const MENU_HEADER_H: f32 = 22.0;
const MENU_ROW_H: f32 = 26.0;
const MENU_SEP_H: f32 = 7.0;
const MENU_H: f32 = MENU_PAD * 2.0 + MENU_HEADER_H + 1.0 + MENU_ROW_H + MENU_SEP_H + MENU_ROW_H;
const MENU_REVEAL_TOP: f32 = MENU_PAD + MENU_HEADER_H + 1.0;
const MENU_DELETE_TOP: f32 = MENU_REVEAL_TOP + MENU_ROW_H + MENU_SEP_H;

// The details view: an Explorer-style table of the open folder's children. The
// columns are fixed widths so the header and every row line up without a real
// table layout, and the name column takes whatever is left.
const DETAIL_ROW_H: f32 = 23.0;
const DETAIL_HEADER_H: f32 = 26.0;
// The navigation pane shared by every view: the volumes, then the open
// folder's chain. `NAV_W` is its width, gap included, when a view reserves room
// for it.
const NAV_W: f32 = 226.0;
/// One folder per row in the navigation tree.
const NAV_ROW_H: f32 = 18.0;
/// Padding between the navigation pane's frame and its rows.
const NAV_PAD: f32 = 8.0;
/// Width of a row when the tree is not scrolled sideways: the pane's frame,
/// minus its border and padding. Volume rows are pinned to it so their usage
/// bar keeps one length however wide a deep folder makes the tree.
const NAV_INNER_W: f32 = NAV_W - 2.0 - NAV_PAD * 2.0;
/// Indentation added per level of the navigation tree.
const NAV_INDENT: f32 = 11.0;
/// Rows the navigation tree will lay out. Expanding a few 500-entry folders
/// would otherwise put tens of thousands of elements in one frame.
const MAX_NAV_ROWS: usize = 2000;
/// Thickness of a scrollbar and the gap it keeps from the frame edge.
const NAV_BAR: f32 = 5.0;
const COL_NAME: f32 = 420.0;
const COL_SIZE: f32 = 92.0;
const COL_FILES: f32 = 70.0;
const COL_FOLDERS: f32 = 70.0;
const COL_STATUS: f32 = 66.0;
const COL_MODIFIED: f32 = 138.0;
const COL_TYPE: f32 = 136.0;
/// How wide or narrow a column can be dragged, so a grip can never make one
/// disappear or swallow the whole table.
const COL_MIN: f32 = 44.0;
const COL_MAX: f32 = 460.0;
/// The name column holds paths, so it gets room to grow beyond COL_MAX.
const NAME_MIN: f32 = 120.0;
const NAME_MAX: f32 = 900.0;
/// Width of the invisible strip on a header's right edge that resizes it.
const COL_GRIP: f32 = 12.0;
/// Thickness of the details table's vertical scrollbar.
const DETAIL_BAR: f32 = 8.0;
/// Two presses on the same row inside this window open it, like Explorer.
const DETAIL_DOUBLE_CLICK: Duration = Duration::from_millis(450);

// The files view: the biggest files in the whole scan, as a table. Rows are a
// fixed height so `uniform_list` can lay out twenty thousand of them without
// building twenty thousand elements.
const FILE_ROW_H: f32 = 24.0;
const FILE_HEADER_H: f32 = 24.0;
const FILE_FOLDER_W: f32 = 320.0;
const FILE_SIZE_W: f32 = 84.0;
const FILE_DATE_W: f32 = 116.0;
const FILE_KIND_W: f32 = 92.0;

// A whole drive can hold thousands of duplicate sets. Show the ones holding the
// most space and say how many were left out, instead of building every row.
const DUP_GROUPS: usize = 200;
const DUP_COPIES: usize = 20;

fn menu_row_at(menu: &Menu, x: f32, y: f32) -> Option<usize> {
    if x < menu.x || x > menu.x + MENU_W || y < menu.y || y > menu.y + MENU_H {
        return None;
    }
    let top = y - menu.y;
    if top >= MENU_REVEAL_TOP && top < MENU_REVEAL_TOP + MENU_ROW_H {
        Some(0)
    } else if top >= MENU_DELETE_TOP && top < MENU_DELETE_TOP + MENU_ROW_H {
        Some(1)
    } else {
        None
    }
}

fn menu_contains(menu: &Menu, x: f32, y: f32) -> bool {
    x >= menu.x && x <= menu.x + MENU_W && y >= menu.y && y <= menu.y + MENU_H
}

/// Sortable columns of the details view.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DetailKey {
    Name,
    Size,
    Files,
    Folders,
    Status,
    Modified,
    Type,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DetailSort {
    key: DetailKey,
    ascending: bool,
}

impl Default for DetailSort {
    fn default() -> Self {
        // Biggest first, which is the question this app exists to answer.
        Self {
            key: DetailKey::Size,
            ascending: false,
        }
    }
}

/// What the filesystem says about one row: the modification time as a value
/// (for sorting) and already formatted (for painting).
#[derive(Clone, Debug)]
struct DetailMeta {
    stamp: Option<SystemTime>,
    label: Option<String>,
}

struct Placed {
    origin: Point<Pixels>,
    geom: Geom,
    /// Treemap / heatmap cells for the frame on screen, indexed like
    /// `ChartModel::slices`. Empty while the sunburst is showing.
    tiles: Arc<Vec<Tile>>,
    view: View,
    /// The chart this layout was measured for. A hit test is only meaningful
    /// against the chart that produced the geometry, so `pick` refuses data
    /// that belongs to a chart no longer on screen.
    chart: Option<Arc<ChartModel>>,
    /// Logical size of the drawing area measured during prepaint.
    size: (f32, f32),
}

impl Placed {
    /// Whether this layout still describes what is on screen.
    ///
    /// The stored geometry is only valid for the view and the chart that
    /// measured it. A treemap frame left behind, or the chart from before a
    /// folder switch, would answer a hit test with slices the user is not
    /// pointing at — so `pick` refuses it instead of guessing.
    fn describes(&self, view: View, chart: &Arc<ChartModel>) -> bool {
        self.view == view
            && self
                .chart
                .as_ref()
                .is_some_and(|measured| Arc::ptr_eq(measured, chart))
    }
}

/// A finished scan kept around so switching back to a drive is instant.
struct Cached {
    tree: Arc<Node>,
    /// The files and kind totals the same walk collected, kept with the tree so
    /// the file and type views are as instant as the chart.
    index: Arc<ScanIndex>,
    elapsed: f64,
    /// Wall-clock time of the walk, so an entry restored from disk can say how
    /// old it is as well as one from earlier in this session.
    at: SystemTime,
}

/// Files view: the column the table is ordered by.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FileSort {
    Name,
    Folder,
    Size,
    Modified,
    Kind,
}

/// Files view: which of the scan's files are listed, and in what order.
///
/// The table holds indices into [`ScanIndex::files`] rather than copies: the
/// list can be twenty thousand rows, and re-sorting offsets is cheap enough to
/// do on every header click.
struct FileTable {
    sort: FileSort,
    ascending: bool,
    /// A kind picked in the Types view, or `None` for the whole scan.
    filter: Option<Category>,
    rows: Vec<usize>,
}

impl Default for FileTable {
    fn default() -> Self {
        // Biggest first, matching the details table and the question the app
        // exists to answer.
        Self {
            sort: FileSort::Size,
            ascending: false,
            filter: None,
            rows: Vec::new(),
        }
    }
}

/// One row of the navigation tree: a volume, or a folder below one.
enum NavEntry {
    Drive {
        drive: DriveInfo,
        expanded: bool,
        has_children: bool,
        /// The volume the chart is currently showing.
        active: bool,
    },
    Folder(NavRow),
}

/// One row of the navigation tree: a folder, how deep it sits, and whether its
/// children are showing.
struct NavRow {
    node: Arc<Node>,
    depth: usize,
    expanded: bool,
    has_children: bool,
}

/// A navigation scrollbar being dragged.
#[derive(Clone, Copy)]
struct NavDrag {
    vertical: bool,
    /// Pointer position along the bar's axis when the drag started.
    origin: f32,
    /// Scroll progress (0 at the top/left, 1 at the end) when it started.
    progress: f32,
}

/// Widths of the details table's fixed columns. The name column takes whatever
/// is left, so only these six can be dragged.
#[derive(Clone, Copy)]
struct Columns {
    name: f32,
    size: f32,
    files: f32,
    folders: f32,
    status: f32,
    modified: f32,
    kind: f32,
}

impl Default for Columns {
    fn default() -> Self {
        Self {
            name: COL_NAME,
            size: COL_SIZE,
            files: COL_FILES,
            folders: COL_FOLDERS,
            status: COL_STATUS,
            modified: COL_MODIFIED,
            kind: COL_TYPE,
        }
    }
}

impl Columns {
    fn of(&self, key: DetailKey) -> f32 {
        match key {
            DetailKey::Name => self.name,
            DetailKey::Size => self.size,
            DetailKey::Files => self.files,
            DetailKey::Folders => self.folders,
            DetailKey::Status => self.status,
            DetailKey::Modified => self.modified,
            DetailKey::Type => self.kind,
        }
    }

    fn set(&mut self, key: DetailKey, width: f32) {
        let width = if key == DetailKey::Name {
            width.clamp(NAME_MIN, NAME_MAX)
        } else {
            width.clamp(COL_MIN, COL_MAX)
        };
        match key {
            DetailKey::Name => self.name = width,
            DetailKey::Size => self.size = width,
            DetailKey::Files => self.files = width,
            DetailKey::Folders => self.folders = width,
            DetailKey::Status => self.status = width,
            DetailKey::Modified => self.modified = width,
            DetailKey::Type => self.kind = width,
        }
    }
}

/// A column grip being dragged, with the width it started from.
#[derive(Clone, Copy)]
struct ColDrag {
    key: DetailKey,
    /// Pointer x when the drag started.
    origin: f32,
    /// Width of the column at that moment.
    from: f32,
}

/// Where the update check stands, from the reader's side: nothing asked yet,
/// asking, a newer release found, fetching it, or fetched and waiting for the
/// restart that installs it.
enum Update {
    Idle,
    Checking,
    /// The running version is the newest release.
    Current,
    Available(Arc<update::Release>),
    Downloading {
        release: Arc<update::Release>,
        /// `(received, total)` bytes, shared with the download's worker.
        progress: Arc<Mutex<(u64, u64)>>,
        cancel: Arc<AtomicBool>,
    },
    Ready {
        release: Arc<update::Release>,
        archive: std::path::PathBuf,
    },
    Failed(String),
}

pub struct DiskApp {
    drives: Vec<DriveInfo>,
    tree: Option<Arc<Node>>,
    focus_path: String,
    chart: Option<Arc<ChartModel>>,
    metric: Metric,
    phase: Phase,
    job: Option<ScanJob>,
    generation: u64,
    progress: (u64, u64, String),
    hover: Option<usize>,
    tooltip: Option<Tooltip>,
    menu: Option<Menu>,
    /// Row under the pointer inside the open context menu (0 reveal, 1 delete).
    menu_hover: Option<usize>,
    placed: Arc<Mutex<Placed>>,
    last_root: String,
    note: Option<String>,
    focus_handle: FocusHandle,
    /// One entry per drive root (lower-cased), so re-visiting a disk repaints
    /// from memory instead of starting from a blank window.
    cache: HashMap<String, Cached>,
    theme_mode: theme::Mode,
    system_dark: bool,
    /// Sunburst, treemap or heatmap — three looks at the same tree.
    view: View,
    /// Canvas size the current tile labels were laid out for.
    tile_size: (f32, f32),
    /// Logical window size from the last frame, for clamping overlays.
    viewport: (f32, f32),
    /// Details view: the column the table is sorted by.
    detail_sort: DetailSort,
    /// Details view: the selected row, and when it was last pressed, so a
    /// second press inside the double-click window opens the folder.
    detail_selected: Option<String>,
    detail_clicked: Option<(String, Instant)>,
    /// Folder tree in the navigation pane: the folders that are open, keyed
    /// like the scan cache so a path compares the same way everywhere.
    nav_expanded: HashSet<String>,
    /// Where that tree is scrolled to, shared with its scrollbars.
    nav_scroll: ScrollHandle,
    /// Logical size of the tree's viewport, measured during paint.
    nav_size: (f32, f32),
    /// The folder the tree was last opened up for, so closing a folder the
    /// chart is inside stays closed instead of being re-opened every frame.
    nav_revealed_for: Option<String>,
    /// A scrollbar being dragged, with where it started.
    nav_drag: Option<NavDrag>,
    /// Details view: where the table is scrolled to, shared with its scrollbar.
    detail_scroll: ScrollHandle,
    /// Details view: the scrollbar being dragged, if any.
    detail_drag: Option<NavDrag>,
    /// Details view: the widths the columns have been dragged to.
    columns: Columns,
    /// The column grip being dragged, if any.
    col_drag: Option<ColDrag>,
    /// Modification dates for the rows on screen, collected on a worker thread.
    detail_meta: Arc<Mutex<HashMap<String, DetailMeta>>>,
    /// Folder whose dates have already been requested, so a frame never starts
    /// the same walk twice.
    detail_meta_for: Option<String>,
    /// Whole-scan file index: the biggest files and the bytes per kind.
    index: Option<Arc<ScanIndex>>,
    /// Files view: sort, filter and the row order built from `index`.
    files: FileTable,
    /// The row order is stale and is rebuilt on the next paint.
    files_dirty: bool,
    /// Files view: the selected row's path, so the toolbar can act on it.
    file_selected: Option<String>,
    /// Files view: the row last pressed, for the double-click window.
    file_clicked: Option<(String, Instant)>,
    /// Files view: where the table is scrolled to.
    file_scroll: UniformListScrollHandle,
    /// A delete changed the type totals; say so instead of showing numbers that
    /// no longer add up until the next full walk.
    types_stale: bool,
    /// The duplicate search, once it has run.
    dupes: Option<Arc<DuplicateScan>>,
    /// The Files view shows the duplicate list instead of the biggest files.
    dupes_visible: bool,
    /// A duplicate search is reading files right now.
    dupes_running: bool,
    /// Which search the running job belongs to; a stale result is dropped.
    dupes_job: Option<u64>,
    dupes_jobs: u64,
    /// Raised to stop the running search.
    dupes_cancel: Option<Arc<AtomicBool>>,
    /// Files read so far, for the progress line.
    dupes_progress: Arc<AtomicU64>,
    /// The update check and download, as the status bar's version chip shows it.
    update: Update,
    /// The update card is open over the window.
    update_open: bool,
}

impl DiskApp {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let drives = list_drives();
        let last_root = load_last_root().unwrap_or_else(|| {
            drives
                .iter()
                .find(|drive| drive.kind == "fixed")
                .map(|drive| drive.root.clone())
                .unwrap_or_else(|| "C:\\".to_string())
        });
        let placed = Arc::new(Mutex::new(Placed {
            origin: point(px(0.), px(0.)),
            geom: Geom::new(600.0, 1),
            tiles: Arc::new(Vec::new()),
            view: View::Rings,
            chart: None,
            size: (0.0, 0.0),
        }));
        let theme_mode = load_theme_mode().unwrap_or(theme::Mode::System);
        let system_dark = !crate::sys::system_prefers_light();
        theme::apply(theme_mode, system_dark);

        // A finished scan left by an earlier session is installed as if it had
        // been walked a moment ago: `scan_into` paints from this map before the
        // walker has done anything, and the walk then refreshes the numbers in
        // place. On a spinning disk that is the difference between a window that
        // draws at once and one that sits blank for minutes.
        let mut cache = HashMap::new();
        if let Some(dir) = snapshot_dir() {
            if let Some(snapshot) = disk_scan::cache::load(&dir, &last_root) {
                cache.insert(
                    cache_key(&last_root),
                    Cached {
                        tree: Node::from_dto(snapshot.tree),
                        index: Arc::new(snapshot.index),
                        elapsed: snapshot.meta.elapsed,
                        at: snapshot.meta.scanned_at,
                    },
                );
            }
        }
        let mut app = DiskApp {
            drives,
            tree: None,
            focus_path: String::new(),
            chart: None,
            metric: Metric::Bytes,
            phase: Phase::Empty,
            job: None,
            generation: 0,
            progress: (0, 0, String::new()),
            hover: None,
            tooltip: None,
            menu: None,
            menu_hover: None,
            placed,
            last_root: last_root.clone(),
            note: None,
            focus_handle: cx.focus_handle(),
            cache,
            theme_mode,
            system_dark,
            view: initial_view(),
            tile_size: (-1.0, -1.0),
            viewport: (1320.0, 880.0),
            detail_sort: DetailSort::default(),
            detail_selected: None,
            detail_clicked: None,
            nav_expanded: HashSet::new(),
            nav_scroll: ScrollHandle::new(),
            nav_size: (-1.0, -1.0),
            nav_revealed_for: None,
            nav_drag: None,
            detail_scroll: ScrollHandle::new(),
            detail_drag: None,
            columns: Columns::default(),
            col_drag: None,
            detail_meta: Arc::new(Mutex::new(HashMap::new())),
            detail_meta_for: None,
            index: None,
            files: FileTable::default(),
            files_dirty: true,
            file_selected: None,
            file_clicked: None,
            file_scroll: UniformListScrollHandle::new(),
            types_stale: false,
            dupes: None,
            dupes_visible: false,
            dupes_running: false,
            dupes_job: None,
            dupes_jobs: 0,
            dupes_cancel: None,
            dupes_progress: Arc::new(AtomicU64::new(0)),
            update: Update::Idle,
            update_open: false,
        };

        // Follow the OS while the skin is on "Auto": the system appearance can
        // change at any time, and the probe is cheap enough to repeat.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs(3))
                .await;
            let changed = this.update(cx, |this, cx| {
                if this.theme_mode != theme::Mode::System {
                    return;
                }
                let now_dark = !crate::sys::system_prefers_light();
                if now_dark != this.system_dark {
                    this.system_dark = now_dark;
                    theme::apply(this.theme_mode, this.system_dark);
                    cx.notify();
                }
            });
            if changed.is_err() {
                break;
            }
        })
        .detach();

        app.start_scan(last_root, cx);
        // The scan comes first; a release check a couple of seconds later only
        // lights the version chip, so nothing jumps in front of the first frame.
        app.check_for_updates_soon(cx);
        app
    }

    fn focus(&self) -> Option<Arc<Node>> {
        let tree = self.tree.as_ref()?;
        if self.focus_path.is_empty() {
            return Some(tree.clone());
        }
        Node::find(tree, &self.focus_path).or_else(|| Some(tree.clone()))
    }

    /// Install a chart for `node`, dropping everything derived from the pointer.
    ///
    /// `hover` and `tooltip` are slice *indices*, which only mean something for
    /// the chart they were picked from: carrying one across a rebuild highlights
    /// a folder nobody is pointing at, and a click on it opens that folder.
    fn show_chart(&mut self, node: Arc<Node>) {
        self.chart = Some(Arc::new(build_chart(node, self.metric)));
        self.clear_hover();
        self.menu = None;
        self.menu_hover = None;
    }

    /// Forget the hovered slice and its tooltip without notifying.
    fn clear_hover(&mut self) {
        self.hover = None;
        self.tooltip = None;
    }

    fn set_focus(&mut self, node: Arc<Node>, cx: &mut Context<Self>) {
        self.focus_path = node.path.clone();
        // A selection belongs to the folder it was made in.
        self.detail_selected = None;
        self.show_chart(node);
        cx.notify();
    }

    /// Jumps the chart to any folder whose absolute path we can resolve, which
    /// is what a click on a breadcrumb segment does. Paths that sit above the
    /// scanned root are not in the tree, so those become a fresh scan of that
    /// folder instead of a dead click.
    fn jump_to(&mut self, path: String, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let mut candidates = vec![path.clone()];
        let trimmed = path.trim_end_matches('\\');
        if trimmed != path {
            candidates.push(trimmed.to_string());
        } else {
            candidates.push(format!("{trimmed}\\"));
        }
        if let Some(node) = candidates
            .iter()
            .find_map(|candidate| find_dir(&tree, candidate))
        {
            self.set_focus(node, cx);
            return;
        }
        // A segment can name the middle of a folder chain the scanner folded
        // into one node (`a\b\c`). The node that *represents* that segment is
        // the nearest one below it, not the deepest: the nearest is the chain
        // itself, and opening it shows the chain's own children.
        if let Some(node) = nearest_below(&tree, &path) {
            self.set_focus(node, cx);
            return;
        }
        if std::path::Path::new(&path).is_dir() {
            self.switch_to(path, cx);
        } else {
            self.note = Some(format!("{} is not in this scan", short_name(&path, 44)));
            cx.notify();
        }
    }

    /// Open every folder above `path`, plus the volume it lives on, so the tree
    /// shows where the chart currently is. Called only when the focus changes,
    /// so a folder the user closed stays closed.
    fn reveal_in_nav(&mut self, path: &str) {
        let Some(tree) = self.tree.clone() else { return };
        if let Some(drive) = self
            .drives
            .iter()
            .find(|drive| {
                drive.root.eq_ignore_ascii_case(path) || is_inside(&drive.root, path)
            })
        {
            self.nav_expanded.insert(cache_key(&drive.root));
        }
        reveal_path(&tree, path, &mut self.nav_expanded);
    }

    /// Open or close one folder's children in the navigation tree. The same
    /// switch opens and closes a volume.
    fn toggle_nav(&mut self, path: &str, cx: &mut Context<Self>) {
        let key = cache_key(path);
        if !self.nav_expanded.remove(&key) {
            self.nav_expanded.insert(key);
        }
        cx.notify();
    }

    /// The rows of the navigation tree: every volume, and the folders below the
    /// ones that are open.
    fn nav_entries(&self) -> Vec<NavEntry> {
        let active_root = self.active_drive().map(|drive| drive.root.clone());
        let mut entries = Vec::new();
        for drive in &self.drives {
            if entries.len() >= MAX_NAV_ROWS {
                break;
            }
            let active = active_root
                .as_deref()
                .is_some_and(|root| root.eq_ignore_ascii_case(&drive.root));
            let tree = self.tree_for(&drive.root);
            let expanded = self.nav_expanded.contains(&cache_key(&drive.root));

            let has_children = tree.as_ref().is_some_and(|node| {
                if node.path.eq_ignore_ascii_case(&drive.root) {
                    node.children.iter().any(|child| child.is_dir())
                } else {
                    // The scan was of a folder inside this volume, and that
                    // folder is the one row the tree knows about.
                    true
                }
            });
            entries.push(NavEntry::Drive {
                drive: drive.clone(),
                expanded: expanded && has_children,
                has_children,
                active,
            });
            let mut rows = Vec::new();
            push_drive_rows(&drive.root, tree.as_ref(), &self.nav_expanded, &mut rows);
            entries.extend(rows.into_iter().map(NavEntry::Folder));
        }
        entries
    }

    /// The tree this volume can draw: the scan in memory, or the one for the
    /// chart on screen when it was a folder inside this volume.
    fn tree_for(&self, root: &str) -> Option<Arc<Node>> {
        if let Some(cached) = self.cache.get(&cache_key(root)) {
            return Some(cached.tree.clone());
        }
        self.tree
            .as_ref()
            .filter(|tree| {
                tree.path.eq_ignore_ascii_case(root) || is_inside(root, &tree.path)
            })
            .cloned()
    }

    /// Thumb length and track length for one scrollbar, or `None` when that
    /// axis has nothing to scroll.
    fn nav_bar(&self, vertical: bool) -> Option<(f32, f32)> {
        let max = self.nav_scroll.max_offset();
        let hidden = if vertical {
            f32::from(max.height)
        } else {
            f32::from(max.width)
        };
        let viewport = if vertical {
            self.nav_size.1
        } else {
            self.nav_size.0
        };
        if viewport <= 1.0 || hidden <= 0.5 {
            return None;
        }
        let track = (viewport - NAV_BAR * 2.0).max(1.0);
        let thumb = (track * viewport / (viewport + hidden)).clamp(track.min(20.0), track);
        Some((thumb, track))
    }

    /// How far down (0 to 1) one axis is scrolled.
    fn nav_progress(&self, vertical: bool) -> f32 {
        let max = self.nav_scroll.max_offset();
        let offset = self.nav_scroll.offset();
        let (hidden, at) = if vertical {
            (f32::from(max.height), -f32::from(offset.y))
        } else {
            (f32::from(max.width), -f32::from(offset.x))
        };
        if hidden <= 0.0 {
            return 0.0;
        }
        (at / hidden).clamp(0.0, 1.0)
    }

    fn begin_nav_drag(&mut self, vertical: bool, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let origin = if vertical {
            f32::from(event.position.y)
        } else {
            f32::from(event.position.x)
        };
        self.nav_drag = Some(NavDrag {
            vertical,
            origin,
            progress: self.nav_progress(vertical),
        });
        cx.stop_propagation();
    }

    /// Follow a navigation scrollbar that is being dragged.
    fn drag_nav(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.nav_drag else { return };
        if event.pressed_button.is_none() {
            self.nav_drag = None;
            return;
        }
        let Some((thumb, track)) = self.nav_bar(drag.vertical) else {
            return;
        };
        let position = if drag.vertical {
            f32::from(event.position.y)
        } else {
            f32::from(event.position.x)
        };
        // Moving the pointer over the free part of the track moves the thumb
        // the same distance, which is the whole content when dragged end to end.
        let range = track - thumb;
        if range <= 0.0 {
            return;
        }
        let progress = (drag.progress + (position - drag.origin) / range).clamp(0.0, 1.0);

        let max = self.nav_scroll.max_offset();
        let current = self.nav_scroll.offset();
        let moved = if drag.vertical {
            let hidden = f32::from(max.height);
            point(current.x, px(-(progress * hidden)))
        } else {
            let hidden = f32::from(max.width);
            point(px(-(progress * hidden)), current.y)
        };
        self.nav_scroll.set_offset(moved);
        cx.notify();
    }

    fn end_nav_drag(&mut self, cx: &mut Context<Self>) {
        if self.nav_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Thumb length and track length for the details table's scrollbar, or
    /// `None` when the rows fit.
    fn detail_bar(&self) -> Option<(f32, f32)> {
        let hidden = f32::from(self.detail_scroll.max_offset().height);
        let viewport = f32::from(self.detail_scroll.bounds().size.height);
        if viewport <= 1.0 || hidden <= 0.5 {
            return None;
        }
        let track = (viewport - DETAIL_BAR * 2.0).max(1.0);
        let thumb = (track * viewport / (viewport + hidden)).clamp(track.min(24.0), track);
        Some((thumb, track))
    }

    /// How far down (0 to 1) the details table is scrolled.
    fn detail_progress(&self) -> f32 {
        let hidden = f32::from(self.detail_scroll.max_offset().height);
        if hidden <= 0.0 {
            return 0.0;
        }
        (-f32::from(self.detail_scroll.offset().y) / hidden).clamp(0.0, 1.0)
    }

    fn begin_detail_drag(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        self.detail_drag = Some(NavDrag {
            vertical: true,
            origin: f32::from(event.position.y),
            progress: self.detail_progress(),
        });
        cx.stop_propagation();
    }

    /// Follow the details scrollbar while it is being dragged.
    fn drag_detail(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.detail_drag else { return };
        if event.pressed_button.is_none() {
            self.detail_drag = None;
            return;
        }
        let Some((thumb, track)) = self.detail_bar() else {
            return;
        };
        let range = track - thumb;
        if range <= 0.0 {
            return;
        }
        let progress =
            (drag.progress + (f32::from(event.position.y) - drag.origin) / range).clamp(0.0, 1.0);
        let hidden = f32::from(self.detail_scroll.max_offset().height);
        let current = self.detail_scroll.offset();
        self.detail_scroll
            .set_offset(point(current.x, px(-(progress * hidden))));
        cx.notify();
    }

    fn end_detail_drag(&mut self, cx: &mut Context<Self>) {
        if self.detail_drag.take().is_some() {
            cx.notify();
        }
    }

    fn begin_col_drag(
        &mut self,
        key: DetailKey,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) {
        self.col_drag = Some(ColDrag {
            key,
            origin: f32::from(event.position.x),
            from: self.columns.of(key),
        });
        // The grip sits on top of the sortable header, so the press must not
        // also sort the table.
        cx.stop_propagation();
    }

    /// Widen or narrow a column while its grip is held.
    fn drag_columns(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.col_drag else { return };
        if event.pressed_button.is_none() {
            self.col_drag = None;
            return;
        }
        let width = drag.from + (f32::from(event.position.x) - drag.origin);
        self.columns.set(drag.key, width);
        cx.notify();
    }

    fn end_col_drag(&mut self, cx: &mut Context<Self>) {
        if self.col_drag.take().is_some() {
            cx.notify();
        }
    }

    /// Open the shell's own context menu for `path` at the pointer.
    ///
    /// The menu is built and run by the shell and blocks until a choice is
    /// made, so it gets a thread of its own (the shell also hands the menu back
    /// through a window that belongs to that thread). Once it closes, a choice
    /// may have renamed, moved or deleted the item, so the open folder is
    /// walked again and the table shows what is actually on disk.
    fn shell_menu(&mut self, path: String, cx: &mut Context<Self>) {
        let done = Arc::new(AtomicBool::new(false));
        let changed = Arc::new(AtomicBool::new(false));
        let (worker_done, worker_changed) = (done.clone(), changed.clone());
        let worker_path = path.clone();
        std::thread::spawn(move || {
            worker_changed.store(crate::sys::context_menu(&worker_path), Ordering::Relaxed);
            worker_done.store(true, Ordering::Relaxed);
        });
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(200))
                .await;
            if !done.load(Ordering::Relaxed) {
                continue;
            }
            if changed.load(Ordering::Relaxed) {
                let _ = this.update(cx, |this, cx| this.after_shell(&path, cx));
            }
            break;
        })
        .detach();
    }

    /// Bring the views back in line after the shell has acted on `path`.
    ///
    /// The shell can rename, move or delete the item behind the menu and it
    /// does not tell anyone which it did. When the path is gone its rows are
    /// dropped from the file index right away; then the open folder is walked
    /// again, which picks up the new names and counts.
    fn after_shell(&mut self, path: &str, cx: &mut Context<Self>) {
        if !Path::new(path).exists() {
            self.drop_index_under(path);
        }
        self.rescan_focus(cx);
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        if self.focus_path.eq_ignore_ascii_case(&tree.path) {
            return;
        }
        match parent_of(&tree, &self.focus_path) {
            Some(node) => self.set_focus(node, cx),
            None => self.set_focus(tree, cx),
        }
    }

    fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        if self.metric == metric {
            return;
        }
        self.metric = metric;
        if let Some(focus) = self.focus() {
            self.show_chart(focus);
        }
        self.clear_hover();
        cx.notify();
    }

    /// Show a volume. A volume already walked in this session is painted from
    /// memory and *not* re-scanned: switching disks should be instant, and a
    /// refresh is the explicit Rescan / F5 action. Only a volume with no scan
    /// yet starts walking straight away.
    fn switch_to(&mut self, root: String, cx: &mut Context<Self>) {
        let key = cache_key(&root);
        if let Some(cached) = self.cache.get(&key) {
                let tree = cached.tree.clone();
                let index = cached.index.clone();
                let elapsed = cached.elapsed;
                let cached_at = cached.at;
                if let Some(job) = &self.job {
                    job.abort();
                }
                self.job = None;
                self.generation += 1;
                self.reset_duplicates();
                let same_root = self.last_root.eq_ignore_ascii_case(&root);
                let focus = if same_root && !self.focus_path.is_empty() {
                    Node::find(&tree, &self.focus_path).unwrap_or_else(|| tree.clone())
                } else {
                    tree.clone()
                };
                self.tree = Some(tree.clone());
                self.index = Some(index);
                self.files_dirty = true;
                self.file_selected = None;
                self.types_stale = false;
                self.focus_path = focus.path.clone();
                self.show_chart(focus);
                self.phase = Phase::Ready {
                    elapsed,
                    files: tree.files,
                    bytes: tree.size,
                };
                self.last_root = root.clone();
                save_last_root(&root);
                self.note = Some(format!(
                    "Showing the scan from {} ago — Rescan to refresh",
                    theme::format_seconds(age_seconds(cached_at))
                ));
                cx.notify();
                return;
        }
        self.start_scan(root, cx);
    }

    /// Rings, treemap, heatmap or the details table over the same tree.
    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view == view {
            cx.notify();
            return;
        }
        self.view = view;
        save_view(view);
        self.hover = None;
        self.tooltip = None;
        cx.notify();
    }

    fn set_theme_mode(&mut self, mode: theme::Mode, cx: &mut Context<Self>) {
        self.theme_mode = mode;
        theme::apply(mode, self.system_dark);
        save_theme_mode(mode);
        cx.notify();
    }

    /// Park the walk in progress: it keeps its counters and partial tree, so
    /// Continue can carry on instead of starting the volume over.
    fn stop_scan(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Scanning) {
            return;
        }
        let Some(job) = self.job.as_mut() else {
            return;
        };
        job.pause.store(true, Ordering::Relaxed);
        if job.paused_since.is_none() {
            job.paused_since = Some(Instant::now());
        }
        self.phase = Phase::Stopped;
        self.note = Some("Continue to resume".to_string());
        cx.notify();
    }

    /// Let the parked walker go on with the counters it already has.
    fn continue_scan(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Stopped) {
            return;
        }
        let Some(job) = self.job.as_mut() else {
            // The job cannot normally vanish while stopped; start the volume
            // again rather than leaving the window stuck if it somehow does.
            let root = self.last_root.clone();
            self.scan_into(root, false, cx);
            return;
        };
        if let Some(since) = job.paused_since.take() {
            job.paused += since.elapsed();
        }
        job.pause.store(false, Ordering::Relaxed);
        self.phase = Phase::Scanning;
        self.note = None;
        cx.notify();
    }

    fn start_scan(&mut self, root: String, cx: &mut Context<Self>) {
        self.scan_into(root, false, cx)
    }

    /// Rescan what the user is looking at: the folder in the focus card when
    /// they have zoomed in, otherwise the whole volume. Refreshing a subfolder
    /// only re-walks that subtree and splices it back into the tree, so it
    /// stays fast on a system drive.
    fn rescan_focus(&mut self, cx: &mut Context<Self>) {
        let root = self.last_root.clone();
        let focus = self.focus_path.clone();
        let inside = !focus.is_empty()
            && !focus.eq_ignore_ascii_case(&root)
            && self.tree.is_some()
            && std::path::Path::new(&focus).is_dir();
        if inside {
            self.scan_into(focus, true, cx);
        } else {
            self.scan_into(root, false, cx);
        }
    }

    fn scan_into(&mut self, target: String, subtree: bool, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.abort();
        }
        self.generation += 1;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(ScanCounters::new());
        self.job = Some(ScanJob {
            cancel: cancel.clone(),
            pause: pause.clone(),
            generation,
            paused: Duration::ZERO,
            paused_since: None,
        });
        self.phase = Phase::Scanning;
        self.progress = (0, 0, target.clone());
        self.clear_hover();
        self.menu = None;
        self.note = None;

        if subtree {
            // Keep the chart and the volume totals; only the pill changes.
            cx.notify();
            self.spawn_scan(target, subtree, generation, cancel, pause, counters, cx);
            return;
        }

        self.reset_duplicates();
        let same_root = self.last_root.eq_ignore_ascii_case(&target);
        self.last_root = target.clone();
        save_last_root(&target);

        // A drive we have already walked is painted from memory straight away;
        // the scan then runs behind it and refreshes the numbers in place.
        if let Some(cached) = self.cache.get(&cache_key(&target)) {
            let tree = cached.tree.clone();
            let index = cached.index.clone();
            let focus = if same_root && !self.focus_path.is_empty() {
                Node::find(&tree, &self.focus_path).unwrap_or_else(|| tree.clone())
            } else {
                tree.clone()
            };
            self.focus_path = focus.path.clone();
            self.show_chart(focus);
            self.tree = Some(tree);
            self.index = Some(index);
            self.files_dirty = true;
            self.file_selected = None;
            self.types_stale = false;
        }
        cx.notify();
        self.spawn_scan(target, subtree, generation, cancel, pause, counters, cx);
    }

    fn spawn_scan(
        &mut self,
        target: String,
        subtree: bool,
        generation: u64,
        cancel: Arc<AtomicBool>,
        pause: Arc<AtomicBool>,
        counters: Arc<ScanCounters>,
        cx: &mut Context<Self>,
    ) {
        // Progress: a cheap snapshot every 120 ms, so the UI stays live while
        // the walker chews through millions of files. While parked there is
        // nothing new to show, so the ticker keeps sleeping without notifying.
        let tick_counters = counters.clone();
        let tick_pause = pause.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    if this.job.as_ref().map(|job| job.generation) != Some(generation) {
                        return false;
                    }
                    if tick_pause.load(Ordering::Relaxed) {
                        return true;
                    }
                    let (files, _, bytes, _, path) = tick_counters.snapshot();
                    this.progress = (files, bytes, path);
                    cx.notify();
                    true
                });
                if !matches!(keep_going, Ok(true)) {
                    break;
                }
            }
        })
        .detach();

        // The walk itself runs on a background thread; the DTO conversion, the
        // snapshot and the Arc tree are built there too.
        let walk_root = target.clone();
        let snapshot_dir = snapshot_dir();
        cx.spawn(async move |this, cx| {
            let work = cx.background_spawn(async move {
                let started = Instant::now();
                match scan_pausable_indexed(Path::new(&walk_root), &cancel, &pause, &counters) {
                    Ok((raw, index)) => {
                        let total = raw.size;
                        let elapsed = started.elapsed().as_secs_f64();
                        let dto = to_dto(&raw, total);
                        // A full, finished walk is what a start-up restores, so
                        // it is worth writing down; a folder refresh only
                        // replaces part of a tree and is left to the next walk.
                        // A superseded walk is not worth keeping either.
                        if !subtree && !cancel.load(Ordering::Relaxed) {
                            if let Some(dir) = &snapshot_dir {
                                let (files, dirs, bytes, errors, _) = counters.snapshot();
                                let meta = ScanMeta {
                                    root: walk_root.clone(),
                                    scanned_at: SystemTime::now(),
                                    elapsed,
                                    files,
                                    dirs,
                                    bytes,
                                    errors,
                                };
                                // A cache write that fails must not fail the
                                // scan: the tree is already in hand.
                                let _ = disk_scan::cache::save(dir, &meta, &dto, &index);
                            }
                        }
                        let tree = Node::from_dto(dto);
                        let cancelled = cancel.load(Ordering::Relaxed);
                        Ok((tree, elapsed, cancelled, index))
                    }
                    Err(error) => Err(error),
                }
            });
            let result = work.await;
            this.update(cx, |this, cx| this.finish_scan(generation, target, subtree, result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_scan(
        &mut self,
        generation: u64,
        target: String,
        subtree: bool,
        result: Result<(Arc<Node>, f64, bool, ScanIndex), String>,
        cx: &mut Context<Self>,
    ) {
        if self.generation != generation {
            return;
        }
        let paused = self
            .job
            .as_ref()
            .map(|job| {
                job.paused
                    + job
                        .paused_since
                        .map(|since| since.elapsed())
                        .unwrap_or_default()
            })
            .unwrap_or_default();
        self.job = None;
        self.note = None;
        match result {
            Ok((tree, elapsed, cancelled, index)) => {
                if cancelled {
                    // This walk was superseded; keep whatever is on screen.
                    match self.cache.get(&cache_key(&self.last_root)) {
                        Some(cached) => {
                            let (elapsed, files, bytes) =
                                (cached.elapsed, cached.tree.files, cached.tree.size);
                            self.phase = Phase::Ready {
                                elapsed,
                                files,
                                bytes,
                            };
                        }
                        None => self.phase = Phase::Empty,
                    }
                    cx.notify();
                    return;
                }

                // Time spent parked on Stop is not time spent scanning.
                let elapsed = (elapsed - paused.as_secs_f64()).max(0.0);

                // A folder refresh only replaces that one subtree and pushes the
                // size difference up through its ancestors.
                if subtree {
                    let merged = self.tree.clone().and_then(|current| {
                        let old = Node::find(&current, &target)?;
                        splice(&current, &target, &tree, Delta::between(&old, &tree))
                    });
                    if let Some(merged) = merged {
                        let focus = Node::find(&merged, &self.focus_path)
                            .unwrap_or_else(|| merged.clone());
                        self.focus_path = focus.path.clone();
                        self.show_chart(focus);
                        self.phase = Phase::Ready {
                            elapsed,
                            files: merged.files,
                            bytes: merged.size,
                        };
                        // The file list keeps the sizes it saw during the full
                        // walk: one refreshed folder is not worth a second pass
                        // over the volume's files.
                        let cached_index = self
                            .index
                            .clone()
                            .unwrap_or_else(|| Arc::new(ScanIndex::default()));
                        self.cache.insert(
                            cache_key(&self.last_root),
                            Cached {
                                tree: merged.clone(),
                                index: cached_index,
                                elapsed,
                                at: SystemTime::now(),
                            },
                        );
                        self.tree = Some(merged);
                        self.files_dirty = true;
                        self.note = Some(format!(
                            "Refreshed {} in {}",
                            short_name(&target, 38),
                            theme::format_seconds(elapsed)
                        ));
                        cx.notify();
                        return;
                    }
                }

                self.focus_path = tree.path.clone();
                self.show_chart(tree.clone());
                self.phase = Phase::Ready {
                    elapsed,
                    files: tree.files,
                    bytes: tree.size,
                };
                // A full walk is the only thing that measures every file and
                // every kind, so it replaces the file and type views outright.
                let index = Arc::new(index);
                self.index = Some(index.clone());
                self.files_dirty = true;
                self.file_selected = None;
                self.types_stale = false;
                self.reset_duplicates();
                self.cache.insert(
                    cache_key(&self.last_root),
                    Cached {
                        tree: tree.clone(),
                        index,
                        elapsed,
                        at: SystemTime::now(),
                    },
                );
                self.tree = Some(tree);
            }
            Err(error) => match self.cache.get(&cache_key(&self.last_root)) {
                Some(cached) => {
                    let (elapsed, files, bytes) =
                        (cached.elapsed, cached.tree.files, cached.tree.size);
                    self.phase = Phase::Ready {
                        elapsed,
                        files,
                        bytes,
                    };
                    self.note = Some(error);
                }
                None => self.phase = Phase::Failed(error),
            },
        }
        cx.notify();
    }

    fn ask_delete(&mut self, node: Arc<Node>, cx: &mut Context<Self>) {
        if node.kind == Kind::Other || node.path.is_empty() {
            return;
        }
        self.menu = None;

        let detail = if node.is_dir() {
            let files = if node.files == 1 {
                "1 file".to_string()
            } else {
                format!("{} files", theme::format_count(node.files))
            };
            format!("{} · {}", theme::format_bytes(node.size), files)
        } else {
            theme::format_bytes(node.size)
        };
        let title = if node.is_dir() {
            "Delete folder"
        } else {
            "Delete file"
        };
        let text = format!(
            "Move this to the Recycle Bin?\n\n{}\n{}\n\nYou can restore it from the Recycle Bin.",
            node.path, detail
        );
        let owner = crate::sys::active_window();
        let path = node.path.clone();

        // The message box is modal to this window and runs on a worker thread so
        // the chart keeps painting behind it.
        cx.spawn(async move |this, cx| {
            let confirmed = cx
                .background_spawn(async move { crate::sys::confirm(owner, title, &text) })
                .await;
            if !confirmed {
                return;
            }
            let target = path.clone();
            let outcome = cx
                .background_spawn(async move { crate::sys::recycle(&target) })
                .await;
            this.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => {
                        this.remove_subtree(&path, cx);
                        this.note = Some(format!(
                            "Moved {} to the Recycle Bin",
                            short_name(&path, 44)
                        ));
                    }
                    Err(error) => this.note = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn remove_subtree(&mut self, target: &str, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let removed = Node::find(&tree, target);
        let pruned = prune(&tree, target);
        let mut focus = self.focus_path.clone();
        if focus.to_lowercase().starts_with(&target.to_lowercase()) {
            focus = tree.path.clone();
        }
        self.tree = Some(pruned.clone());
        self.focus_path = focus;
        // The status bar shows what the walk found; a delete makes that stale.
        if let (Some(removed), Phase::Ready { files, bytes, .. }) = (removed, &mut self.phase) {
            *files = files.saturating_sub(removed.files);
            *bytes = bytes.saturating_sub(removed.size);
        }
        let node = Node::find(&pruned, &self.focus_path).unwrap_or(pruned);
        self.show_chart(node);
        self.drop_index_under(target);
        cx.notify();
    }

    /// Forget the indexed files a delete removed.
    ///
    /// A Recycle Bin delete changes the volume without a walk, so the file list
    /// and the type totals would keep counting the files that are gone. The
    /// bytes per type are only corrected by the next full scan (they are merged
    /// atomically during the walk), which `types_stale` admits on screen.
    fn drop_index_under(&mut self, target: &str) {
        let Some(index) = self.index.clone() else {
            return;
        };
        let gone = |path: &str| is_inside(target, path) || path.eq_ignore_ascii_case(target);
        let mut files = index.files.clone();
        let before = files.len();
        files.retain(|hit| !gone(&hit.path));
        if files.len() != before {
            let mut big = index.big.clone();
            big.retain(|hit| !gone(&hit.path));
            self.index = Some(Arc::new(ScanIndex {
                files,
                big,
                buckets: index.buckets.clone(),
            }));
            self.files_dirty = true;
            self.types_stale = true;
            if let Some(scan) = self.dupes.clone() {
                // The deleted copies are gone, and a set that is down to one
                // copy is not a duplicate any more.
                self.dupes = Some(Arc::new(prune_duplicate_copies(&scan, target)));
            }
            if self
                .file_selected
                .as_deref()
                .is_some_and(|path| is_inside(target, path))
            {
                self.file_selected = None;
            }
        }
    }

    // -------------------------------------------------------- files and types

    /// Put the Files view's row order back in sync with its sort and filter.
    fn rebuild_files(&mut self) {
        self.files_dirty = false;
        let Some(index) = self.index.clone() else {
            self.files.rows.clear();
            return;
        };
        let filter = self.files.filter;
        let mut rows: Vec<usize> = index
            .files
            .iter()
            .enumerate()
            .filter(|(_, hit)| match filter {
                Some(category) => Category::of_name(file_name(&hit.path)) == category,
                None => true,
            })
            .map(|(offset, _)| offset)
            .collect();
        let sort = self.files.sort;
        let ascending = self.files.ascending;
        rows.sort_by(|&left, &right| {
            let (left, right) = (&index.files[left], &index.files[right]);
            let order = match sort {
                FileSort::Name => file_name(&left.path).cmp(file_name(&right.path)),
                FileSort::Folder => file_folder(&left.path).cmp(file_folder(&right.path)),
                FileSort::Size => left.size.cmp(&right.size),
                FileSort::Modified => left.modified.cmp(&right.modified),
                FileSort::Kind => Category::of_name(file_name(&left.path))
                    .label()
                    .cmp(Category::of_name(file_name(&right.path)).label()),
            };
            let order = if ascending { order } else { order.reverse() };
            order.then_with(|| left.path.cmp(&right.path))
        });
        self.files.rows = rows;
    }

    /// Sort the Files table, reversing when the active column is picked again.
    fn set_file_sort(&mut self, sort: FileSort, cx: &mut Context<Self>) {
        if self.files.sort == sort {
            self.files.ascending = !self.files.ascending;
        } else {
            self.files.sort = sort;
            // Sizes and dates are asked for biggest/newest first; names and
            // kinds read better from A to Z.
            self.files.ascending =
                matches!(sort, FileSort::Name | FileSort::Folder | FileSort::Kind);
        }
        self.files_dirty = true;
        cx.notify();
    }

    /// Show one kind of file in the Files view, or every file again.
    fn filter_files(&mut self, category: Option<Category>, cx: &mut Context<Self>) {
        self.files.filter = category;
        self.files.sort = FileSort::Size;
        self.files.ascending = false;
        self.files_dirty = true;
        if self.view != View::Files {
            self.view = View::Files;
            save_view(self.view);
        }
        cx.notify();
    }

    /// The selected file, if the selection still names a listed file.
    fn selected_file(&self) -> Option<FileHit> {
        let path = self.file_selected.as_deref()?;
        let index = self.index.as_ref()?;
        index
            .files
            .iter()
            .find(|hit| hit.path.eq_ignore_ascii_case(path))
            .cloned()
    }

    /// A press on a file row: select it, and open its folder on a double press.
    fn pick_file(&mut self, hit: FileHit, cx: &mut Context<Self>) {
        let now = Instant::now();
        let double = self.file_clicked.as_ref().is_some_and(|(path, at)| {
            now.duration_since(*at) <= DETAIL_DOUBLE_CLICK && path.eq_ignore_ascii_case(&hit.path)
        });
        self.file_selected = Some(hit.path.clone());
        self.file_clicked = Some((hit.path.clone(), now));
        if double {
            self.open_containing(&hit, cx);
        }
        cx.notify();
    }

    /// Jump to the folder that holds a file, in the details table.
    fn open_containing(&mut self, hit: &FileHit, cx: &mut Context<Self>) {
        let folder = file_folder(&hit.path).to_string();
        if folder.is_empty() {
            return;
        }
        if self.view != View::Details {
            self.view = View::Details;
            save_view(self.view);
        }
        self.jump_to(folder, cx);
    }

    fn reveal_file(&mut self, hit: &FileHit, cx: &mut Context<Self>) {
        let _ = crate::sys::reveal(&hit.path, false);
        self.note = Some(format!("Revealed {}", short_name(&hit.path, 44)));
        cx.notify();
    }

    /// Write the listed rows to a CSV, so the biggest files can leave the app.
    fn export_files(&mut self, cx: &mut Context<Self>) {
        let Some(index) = self.index.clone() else {
            return;
        };
        let rows: Vec<FileHit> = self
            .files
            .rows
            .iter()
            .filter_map(|&row| index.files.get(row).cloned())
            .collect();
        if rows.is_empty() {
            return;
        }
        let name = format!(
            "disk-scanner-files-{}.csv",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0)
        );
        let path = export_dir().join(name);
        let written = path.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { write_file_csv(&path, &rows) })
                .await;
            this.update(cx, |this, cx| {
                this.note = Some(match outcome {
                    Ok(()) => format!(
                        "Wrote {}",
                        short_name(&written.display().to_string(), 48)
                    ),
                    Err(error) => format!("Could not write the CSV: {error}"),
                });
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Drop the duplicate results: the index they were grouped from changed.
    fn reset_duplicates(&mut self) {
        self.dupes = None;
        self.dupes_visible = false;
        self.dupes_running = false;
        self.dupes_job = None;
        if let Some(cancel) = self.dupes_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Start comparing files for identical contents. Only the big files the
    /// index kept a path for take part, so a folder of small icons cannot make
    /// the search crawl, and the result only needs read access.
    fn start_duplicates(&mut self, cx: &mut Context<Self>) {
        if self.dupes_running {
            return;
        }
        let Some(index) = self.index.clone() else {
            return;
        };
        let files = index.big.clone();
        self.dupes_visible = true;
        if files.is_empty() {
            self.dupes = Some(Arc::new(DuplicateScan::default()));
            cx.notify();
            return;
        }
        self.dupes = None;
        self.dupes_running = true;
        self.dupes_jobs += 1;
        let job = self.dupes_jobs;
        self.dupes_job = Some(job);
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        self.dupes_cancel = Some(cancel.clone());
        self.dupes_progress = progress.clone();

        // Repaint while the hashing runs, so the count in the title moves.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let keep = this.update(cx, |this, cx| {
                if this.dupes_job == Some(job) && this.dupes_running {
                    cx.notify();
                    true
                } else {
                    false
                }
            });
            if !keep.unwrap_or(false) {
                break;
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let scan = cx
                .background_spawn(async move { find_duplicates(&files, &cancel, &progress) })
                .await;
            this.update(cx, |this, cx| {
                if this.dupes_job == Some(job) {
                    this.dupes = Some(Arc::new(scan));
                    this.dupes_running = false;
                    this.dupes_job = None;
                    this.dupes_cancel = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Stop a running search but stay on the duplicates list.
    fn cancel_duplicates(&mut self, cx: &mut Context<Self>) {
        if let Some(cancel) = self.dupes_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.dupes_running = false;
        self.dupes_job = None;
        self.dupes = None;
        cx.notify();
    }

    /// Leave the duplicate list and go back to the biggest files.
    fn stop_duplicates(&mut self, cx: &mut Context<Self>) {
        self.cancel_duplicates(cx);
        self.dupes_visible = false;
        cx.notify();
    }

    fn dupes_header(&self) -> Div {
        let cell = |text: &'static str, width: Option<f32>, right: bool| {
            let mut cell = div()
                .flex_shrink_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_size(px(9.5))
                .text_color(theme::text_faint());
            match width {
                Some(width) => {
                    cell = cell.w(px(width));
                    if right {
                        cell = cell.text_right();
                    }
                }
                None => cell = cell.flex_1().min_w_0(),
            }
            cell.child(text)
        };
        div()
            .flex()
            .items_center()
            .h(px(FILE_HEADER_H))
            .gap(px(8.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(theme::hairline())
            .child(div().w(px(7.0)))
            .child(cell("Copy", None, false))
            .child(cell("Folder", Some(FILE_FOLDER_W), false))
            .child(cell("Modified", Some(FILE_DATE_W), true))
    }

    /// The duplicate list: one strip per set of identical files, then the copies
    /// themselves. Rows are built by hand here because the sets have two row
    /// heights and `uniform_list` needs one.
    fn dupes_body(&self, cx: &Context<Self>) -> AnyElement {
        if self.dupes_running {
            let hashed = self.dupes_progress.load(Ordering::Relaxed);
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.0))
                .text_size(px(11.0))
                .text_color(theme::text_dim())
                .child(format!(
                    "Comparing files… {} read so far.",
                    theme::format_count(hashed)
                ))
                .into_any_element();
        }
        let Some(scan) = self.dupes.as_ref() else {
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.0))
                .text_size(px(11.0))
                .text_color(theme::text_dim())
                .child("The search stopped before it finished. Pick Find duplicates to run it again.")
                .into_any_element();
        };
        if scan.groups.is_empty() {
            let what = if scan.cancelled {
                "the search stopped early"
            } else {
                "no two files of 1 MB or more hold the same bytes"
            };
            return div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .px(px(24.0))
                .text_size(px(11.0))
                .text_color(theme::text_dim())
                .child(format!(
                    "Nothing to show: {what} ({} files compared).",
                    theme::format_count(scan.hashed as u64)
                ))
                .into_any_element();
        }

        let selected = self.file_selected.as_deref();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut hidden = 0usize;
        for (group_index, group) in scan.groups.iter().enumerate() {
            if group_index >= DUP_GROUPS {
                hidden += 1;
                continue;
            }
            rows.push(Self::dup_group_row(group));
            for (copy_index, copy) in group.copies.iter().take(DUP_COPIES).enumerate() {
                let selected =
                    selected.is_some_and(|path| path.eq_ignore_ascii_case(&copy.path));
                rows.push(Self::dup_copy_row(group_index, copy_index, copy, selected, cx));
            }
            if group.copies.len() > DUP_COPIES {
                rows.push(
                    div()
                        .px(px(18.0))
                        .pb(px(4.0))
                        .text_size(px(10.0))
                        .text_color(theme::text_faint())
                        .child(format!(
                            "…and {} more copies of this file",
                            group.copies.len() - DUP_COPIES
                        ))
                        .into_any_element(),
                );
            }
        }
        let mut list = div()
            .id("dupes-rows")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col();
        for row in rows {
            list = list.child(row);
        }
        if hidden > 0 {
            list = list.child(
                div()
                    .px(px(10.0))
                    .py(px(8.0))
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(format!(
                        "…and {hidden} more sets. Delete some copies and run the search again to see them."
                    )),
            );
        }
        list.into_any_element()
    }

    fn dup_group_row(group: &disk_scan::DupGroup) -> AnyElement {
        let wasted = group.size * group.copies.len().saturating_sub(1) as u64;
        div()
            .h(px(22.0))
            .flex_shrink_0()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .text_size(px(9.5))
            .text_color(theme::text_dim())
            .child(
                div()
                    .w(px(7.0))
                    .h(px(7.0))
                    .flex_shrink_0()
                    .rounded(px(3.5))
                    .bg(theme::accent()),
            )
            .child(format!(
                "{} · {} each · {} wasted",
                count_label(group.copies.len(), "copy", "copies"),
                theme::format_bytes(group.size),
                theme::format_bytes(wasted)
            ))
            .child(div().flex_1())
            .into_any_element()
    }

    fn dup_copy_row(
        group: usize,
        copy_index: usize,
        hit: &FileHit,
        selected: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let click = hit.clone();
        let right_click = hit.path.clone();
        let mut line = div()
            .id(SharedString::from(format!("dup-{group}-{copy_index}")))
            .h(px(FILE_ROW_H))
            .flex_shrink_0()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .cursor_pointer()
            .bg(if selected {
                theme::accent_soft()
            } else {
                theme::surface()
            })
            .text_size(px(11.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.pick_file(click.clone(), cx);
                }),
            );
        if !selected {
            line = line.hover(|style| style.bg(theme::surface_alt()));
        }
        line.child(div().w(px(7.0)).flex_shrink_0())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .pl(px(12.0))
                    .text_color(theme::text())
                    .child(file_name(&hit.path).to_string()),
            )
            .child(
                div()
                    .w(px(FILE_FOLDER_W))
                    .flex_shrink_0()
                    .truncate()
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(short_folder(file_folder(&hit.path), 46)),
            )
            .child(
                div()
                    .w(px(FILE_DATE_W))
                    .flex_shrink_0()
                    .text_right()
                    .text_size(px(10.0))
                    .text_color(theme::text_dim())
                    .child(format_index_date(hit.modified)),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.file_selected = Some(right_click.clone());
                    cx.notify();
                    this.shell_menu(right_click.clone(), cx);
                }),
            )
            .into_any_element()
    }

    /// The biggest files of the whole scan, virtualised: sorting twenty
    /// thousand offsets is cheap, building twenty thousand rows is not.
    fn files_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.index.is_none() {
            return self
                .waiting_card("Files", &self.waiting_message())
                .into_any_element();
        }
        if self.files_dirty {
            self.rebuild_files();
        }
        let shown = self.files.rows.len();
        let filter = self.files.filter;
        let selected = self.selected_file();
        let scroll = self.file_scroll.clone();
        let total_bytes = self
            .index
            .as_ref()
            .map(|index| index.total().bytes)
            .unwrap_or(0);
        let total_files = self
            .index
            .as_ref()
            .map(|index| index.total().files)
            .unwrap_or(0);

        let processor = cx.processor(
            move |this: &mut Self,
                  range: std::ops::Range<usize>,
                  _window: &mut Window,
                  cx: &mut Context<Self>| {
                let Some(index) = this.index.clone() else {
                    return Vec::new();
                };
                let mut rows: Vec<AnyElement> = Vec::with_capacity(range.len());
                for row in range {
                    let Some(&offset) = this.files.rows.get(row) else {
                        continue;
                    };
                    let Some(hit) = index.files.get(offset) else {
                        continue;
                    };
                    let selected = this
                        .file_selected
                        .as_deref()
                        .is_some_and(|path| path.eq_ignore_ascii_case(&hit.path));
                    rows.push(Self::file_row(row, hit, selected, cx));
                }
                rows
            },
        );

        let dupes_mode = self.dupes_visible;
        let dupes_count = if self.dupes_running {
            format!(
                "Comparing {} files…",
                theme::format_count(self.dupes_progress.load(Ordering::Relaxed))
            )
        } else {
            match self.dupes.as_ref() {
                Some(scan) if !scan.groups.is_empty() => format!(
                    "{} · {} · {} wasted",
                    count_label(scan.groups.len(), "set", "sets"),
                    count_label(scan.copies(), "copy", "copies"),
                    theme::format_bytes(scan.wasted())
                ),
                Some(scan) => format!(
                    "{} files compared · nothing repeated",
                    theme::format_count(scan.hashed as u64)
                ),
                None => "no search yet".to_string(),
            }
        };
        let count = if dupes_mode {
            dupes_count
        } else {
            match filter {
                Some(category) => format!(
                    "{} {} files · {}",
                    theme::format_count(shown as u64),
                    category.label().to_lowercase(),
                    theme::format_bytes(total_bytes)
                ),
                None if shown as u64 >= total_files => format!(
                    "{} files · {}",
                    theme::format_count(total_files),
                    theme::format_bytes(total_bytes)
                ),
                None => format!(
                    "Biggest {} of {} files · {}",
                    theme::format_count(shown as u64),
                    theme::format_count(total_files),
                    theme::format_bytes(total_bytes)
                ),
            }
        };

        let mut title = div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .h(px(38.0))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme::hairline())
            .child(
                div()
                    .text_size(px(12.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Files"),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme::text_dim())
                    .child(count),
            )
            .child(div().flex_1());
        if let Some(category) = filter.filter(|_| !dupes_mode) {
            title = title.child(
                div()
                    .id("files-clear-filter")
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(7.0))
                    .py(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .bg(theme::accent_soft())
                    .text_size(px(10.5))
                    .text_color(theme::text())
                    .hover(|style| style.bg(theme::surface_alt()))
                    .on_click(cx.listener(|this, _, _, cx| this.filter_files(None, cx)))
                    .child(format!("{} ✕", category.label())),
            );
        }
        let (dupes_label, dupes_id, dupes_active) = if self.dupes_running {
            ("Stop", "files-dupes-stop", true)
        } else if dupes_mode {
            ("Duplicates ✕", "files-dupes-clear", true)
        } else {
            ("Find duplicates", "files-dupes", false)
        };
        title = title.child(
            div()
                .id(dupes_id)
                .flex()
                .items_center()
                .px(px(7.0))
                .py(px(3.0))
                .rounded(px(5.0))
                .cursor_pointer()
                .bg(if dupes_active {
                    theme::accent_soft()
                } else {
                    theme::surface_alt()
                })
                .text_size(px(10.5))
                .text_color(if dupes_active {
                    theme::text()
                } else {
                    theme::text_dim()
                })
                .hover(|style| style.bg(theme::surface_alt()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.dupes_running {
                        this.cancel_duplicates(cx);
                    } else if this.dupes_visible {
                        this.stop_duplicates(cx);
                    } else {
                        this.start_duplicates(cx);
                    }
                }))
                .child(dupes_label),
        );
        if !dupes_mode {
            title = title.child(self.ghost_button("Export CSV", shown > 0, cx, |this, cx| {
                this.export_files(cx)
            }));
        }

        let mut actions = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(12.0))
            .h(px(36.0))
            .flex_shrink_0()
            .border_b_1()
            .border_color(theme::hairline());
        match &selected {
            Some(hit) => {
                actions = actions
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(10.5))
                            .text_color(theme::text_dim())
                            .child(hit.path.clone()),
                    )
                    .child(self.ghost_button("Show in folder", true, cx, {
                        let hit = hit.clone();
                        move |this, cx| this.open_containing(&hit, cx)
                    }))
                    .child(self.action_button("↗ Reveal", cx, {
                        let hit = hit.clone();
                        move |this, cx| this.reveal_file(&hit, cx)
                    }))
                    .child(self.danger_button("🗑 Delete…", true, cx, {
                        let hit = hit.clone();
                        move |this, cx| this.ask_delete(file_node(&hit), cx)
                    }));
            }
            None => {
                actions = actions.child(
                    div()
                        .flex_1()
                        .text_size(px(10.5))
                        .text_color(theme::text_faint())
                        .child(if dupes_mode {
                            "Pick a copy to reveal it or delete it. Keep at least one copy of each set."
                        } else {
                            "Pick a row to open its folder, reveal it or move it to the Recycle Bin."
                        }),
                );
            }
        }

        let body: AnyElement = if dupes_mode {
            self.dupes_body(cx)
        } else if shown == 0 {
            div()
                .flex_1()
                .min_h_0()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.5))
                .text_color(theme::text_dim())
                .child(if filter.is_some() {
                    "No files of this kind in the scan."
                } else {
                    "The scan did not find any files."
                })
                .into_any_element()
        } else {
            uniform_list("files-rows", shown, processor)
                .track_scroll(scroll)
                .flex_1()
                .min_h_0()
                .into_any_element()
        };

        let order = match self.files.sort {
            FileSort::Name => "name",
            FileSort::Folder => "folder",
            FileSort::Size => "size",
            FileSort::Modified => "modified",
            FileSort::Kind => "kind",
        };

        let footer_left = if dupes_mode {
            "Only files of 1 MB and up are compared, by their bytes, not their names."
        } else {
            "Double-click a row to open its folder. Files are matched by name."
        };
        let footer_right = if dupes_mode {
            "Sorted by space wasted".to_string()
        } else {
            format!(
                "Sorted by {order} {}",
                if self.files.ascending { "↑" } else { "↓" }
            )
        };

        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .overflow_hidden()
            .child(title)
            .child(actions)
            .child(if dupes_mode {
                self.dupes_header()
            } else {
                self.files_header(cx)
            })
            .child(body)
            .child(
                div()
                    .h(px(24.0))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(12.0))
                    .border_t_1()
                    .border_color(theme::hairline())
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(footer_left)
                    .child(div().flex_1())
                    .child(footer_right),
            )
            .into_any_element()
    }

    fn files_header(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .h(px(FILE_HEADER_H))
            .flex_shrink_0()
            .gap(px(8.0))
            .px(px(10.0))
            .border_b_1()
            .border_color(theme::hairline())
            .text_size(px(9.5))
            .text_color(theme::text_faint())
            // Gutter for the kind dot every row draws first.
            .child(div().w(px(7.0)).flex_shrink_0())
            .child(self.file_sort_cell("Name", FileSort::Name, None, false, cx))
            .child(self.file_sort_cell(
                "Folder",
                FileSort::Folder,
                Some(FILE_FOLDER_W),
                false,
                cx,
            ))
            .child(self.file_sort_cell(
                "Size",
                FileSort::Size,
                Some(FILE_SIZE_W),
                true,
                cx,
            ))
            .child(self.file_sort_cell(
                "Modified",
                FileSort::Modified,
                Some(FILE_DATE_W),
                true,
                cx,
            ))
            .child(self.file_sort_cell(
                "Kind",
                FileSort::Kind,
                Some(FILE_KIND_W),
                true,
                cx,
            ))
    }

    fn file_sort_cell(
        &self,
        label: &'static str,
        sort: FileSort,
        width: Option<f32>,
        right: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = self.files.sort == sort;
        let arrow = if active {
            if self.files.ascending {
                " ▲"
            } else {
                " ▼"
            }
        } else {
            ""
        };
        let mut cell = div()
            .id(SharedString::from(format!("file-sort-{label}")))
            .cursor_pointer()
            .text_color(if active {
                theme::text()
            } else {
                theme::text_faint()
            })
            .hover(|style| style.text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| this.set_file_sort(sort, cx)));
        match width {
            Some(width) => {
                cell = cell.w(px(width)).flex_shrink_0();
                if right {
                    cell = cell.text_right();
                }
            }
            None => cell = cell.flex_1().min_w_0(),
        }
        cell.child(format!("{label}{arrow}"))
    }

    /// One row of the file table. The row is built from the hit rather than the
    /// tree, because these files are not all in it: the list is scan-wide.
    fn file_row(
        row: usize,
        hit: &FileHit,
        selected: bool,
        cx: &Context<Self>,
    ) -> AnyElement {
        let name = file_name(&hit.path);
        let category = Category::of_name(name);
        let right_click = hit.path.clone();
        let click = hit.clone();
        let mut line = div()
            .id(SharedString::from(format!("file-row-{row}")))
            .h(px(FILE_ROW_H))
            .flex_shrink_0()
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(10.0))
            .cursor_pointer()
            .bg(if selected {
                theme::accent_soft()
            } else {
                theme::surface()
            })
            .text_size(px(11.0))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                    this.pick_file(click.clone(), cx);
                }),
            );
        if !selected {
            line = line.hover(|style| style.bg(theme::surface_alt()));
        }
        line.child(
            div()
                .w(px(7.0))
                .h(px(7.0))
                .flex_shrink_0()
                .rounded(px(2.0))
                .bg(category_color(category)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::text())
                .child(name.to_string()),
        )
        .child(
            div()
                .w(px(FILE_FOLDER_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(10.0))
                .text_color(theme::text_faint())
                .child(short_folder(file_folder(&hit.path), 46)),
        )
        .child(
            div()
                .w(px(FILE_SIZE_W))
                .flex_shrink_0()
                .text_right()
                .font_weight(FontWeight::MEDIUM)
                .child(theme::format_bytes(hit.size)),
        )
        .child(
            div()
                .w(px(FILE_DATE_W))
                .flex_shrink_0()
                .text_size(px(10.0))
                .text_color(theme::text_dim())
                .child(format_index_date(hit.modified)),
        )
        .child(
            div()
                .w(px(FILE_KIND_W))
                .flex_shrink_0()
                .truncate()
                .text_size(px(10.0))
                .text_color(theme::text_dim())
                .child(category.label().to_string()),
        )
        .on_mouse_up(
            MouseButton::Right,
            cx.listener(move |this, _, _, cx| {
                this.file_selected = Some(right_click.clone());
                cx.notify();
                this.shell_menu(right_click.clone(), cx);
            }),
        )
        .into_any_element()
    }

    /// What the whole scan holds, split by the kind of file it is. A row filters
    /// the file list, so "video takes 400 GB" is one click from "which videos".
    fn types_area(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(index) = self.index.clone() else {
            return self
                .waiting_card("Types", &self.waiting_message())
                .into_any_element();
        };
        let total = index.total();
        let mut rows: Vec<(Category, u64, u64)> = index
            .buckets
            .iter()
            .filter(|(_, bucket)| bucket.files > 0)
            .map(|(category, bucket)| (category, bucket.bytes, bucket.files))
            .collect();
        rows.sort_by(|left, right| right.1.cmp(&left.1));
        let max = rows.first().map(|row| row.1).unwrap_or(0).max(1);

        let mut list = div()
            .id("types-rows")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (category, bytes, files) in rows {
            list = list.child(self.type_row(category, bytes, files, total.bytes, max, cx));
        }

        let mut footer = div()
            .h(px(26.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .border_t_1()
            .border_color(theme::hairline())
            .text_size(px(10.0))
            .text_color(theme::text_faint())
            .child("By file name, so a renamed extension counts as what it says.");
        if self.types_stale {
            footer = footer.child(div().flex_1()).child(
                div()
                    .text_color(theme::danger())
                    .child("Deleted files are still counted until the next full scan."),
            );
        }

        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .px(px(12.0))
                    .h(px(38.0))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme::hairline())
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Types"),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme::text_dim())
                            .child(format!(
                                "{} across {} files",
                                theme::format_bytes(total.bytes),
                                theme::format_count(total.files)
                            )),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme::text_faint())
                            .child("Click a kind to list its files."),
                    ),
            )
            .child(list)
            .child(footer)
            .into_any_element()
    }

    fn type_row(
        &self,
        category: Category,
        bytes: u64,
        files: u64,
        total: u64,
        max: u64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let share = if total > 0 {
            bytes as f64 / total as f64
        } else {
            0.0
        };
        let ratio = if max > 0 {
            (bytes as f64 / max as f64) as f32
        } else {
            0.0
        };
        let color = category_color(category);
        div()
            .id(SharedString::from(format!("type-{}", category.slug())))
            .h(px(44.0))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(10.0))
            .px(px(12.0))
            .border_b_1()
            .border_color(theme::hairline())
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_alt()))
            .on_click(cx.listener(move |this, _, _, cx| this.filter_files(Some(category), cx)))
            .child(
                div()
                    .w(px(10.0))
                    .h(px(10.0))
                    .flex_shrink_0()
                    .rounded(px(3.0))
                    .bg(color),
            )
            .child(
                div()
                    .w(px(150.0))
                    .flex_shrink_0()
                    .text_size(px(11.5))
                    .font_weight(FontWeight::MEDIUM)
                    .child(category.label().to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .h(px(6.0))
                            .rounded_full()
                            .bg(theme::track())
                            .child(
                                div()
                                    .h(px(6.0))
                                    .rounded_full()
                                    .w(relative(ratio.clamp(0.004, 1.0)))
                                    .bg(color),
                            ),
                    )
                    .child(
                        div()
                            .w(px(52.0))
                            .flex_shrink_0()
                            .text_right()
                            .text_size(px(10.5))
                            .text_color(theme::text_dim())
                            .child(theme::format_percent(share)),
                    ),
            )
            .child(
                div()
                    .w(px(88.0))
                    .flex_shrink_0()
                    .text_right()
                    .text_size(px(11.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(theme::format_bytes(bytes)),
            )
            .child(
                div()
                    .w(px(80.0))
                    .flex_shrink_0()
                    .text_right()
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(format!("{} files", theme::format_count(files))),
            )
    }

    /// The card shown when a view has nothing to draw yet.
    fn waiting_card(&self, title: &str, message: &str) -> Div {
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(430.0))
                    .rounded(px(12.0))
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::hairline())
                    .p(px(18.0))
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title.to_string()),
                    )
                    .child(
                        div()
                            .text_size(px(11.0))
                            .text_color(theme::text_dim())
                            .child(message.to_string()),
                    ),
            )
    }

    fn waiting_message(&self) -> String {
        match &self.phase {
            Phase::Failed(error) => format!("The last scan failed: {error}"),
            Phase::Scanning | Phase::Stopped => {
                "Counting files — this view is ready as soon as the walk finishes.".to_string()
            }
            _ => "Run a scan to see the volume's files here.".to_string(),
        }
    }

    // ----------------------------------------------------------- details view

    fn set_detail_sort(&mut self, key: DetailKey, cx: &mut Context<Self>) {
        if self.detail_sort.key == key {
            self.detail_sort.ascending = !self.detail_sort.ascending;
        } else {
            self.detail_sort = DetailSort {
                key,
                ascending: matches!(key, DetailKey::Name | DetailKey::Type | DetailKey::Status),
            };
        }
        cx.notify();
    }

    /// A press in the table selects the row; a second press on the same row
    /// inside the double-click window opens it, exactly like Explorer.
    fn detail_click(&mut self, node: Arc<Node>, cx: &mut Context<Self>) {
        let path = node.path.clone();
        let again = self
            .detail_clicked
            .as_ref()
            .is_some_and(|(last, at)| last == &path && at.elapsed() < DETAIL_DOUBLE_CLICK);
        self.detail_clicked = Some((path.clone(), Instant::now()));
        self.detail_selected = Some(path);
        if again {
            self.detail_open(node, cx);
        } else {
            cx.notify();
        }
    }

    /// Open a table row: a folder becomes the new focus; a file, or an
    /// aggregated row that has no path, is reported in the status bar.
    fn detail_open(&mut self, node: Arc<Node>, cx: &mut Context<Self>) {
        if node.is_dir() && !node.path.is_empty() {
            self.detail_selected = None;
            self.set_focus(node, cx);
        } else if !node.path.is_empty() {
            self.note = Some(format!(
                "{} · {}",
                node.name,
                theme::format_bytes(node.size)
            ));
            cx.notify();
        }
    }

    /// The row the toolbar buttons act on.
    fn selected_detail_node(&self) -> Option<Arc<Node>> {
        let path = self.detail_selected.as_ref()?;
        let focus = self.focus()?;
        focus
            .children
            .iter()
            .find(|child| child.path.eq_ignore_ascii_case(path))
            .cloned()
    }

    /// Ask a worker thread for the modification dates of the open folder's
    /// children. Reading them in the render pass would stall the window on a
    /// slow volume, so the table shows its placeholders until they arrive.
    fn request_detail_meta(&mut self, focus: &Arc<Node>, cx: &mut Context<Self>) {
        let key = cache_key(&focus.path);
        if self.detail_meta_for.as_deref() == Some(key.as_str()) {
            return;
        }
        self.detail_meta_for = Some(key.clone());
        let paths: Vec<String> = focus
            .children
            .iter()
            .filter(|child| !child.path.is_empty())
            .map(|child| child.path.clone())
            .collect();
        let store = self.detail_meta.clone();
        cx.spawn(async move |this, cx| {
            let dates = cx
                .background_spawn(async move {
                    let mut out: Vec<(String, Option<(SystemTime, String)>)> =
                        Vec::with_capacity(paths.len());
                    for path in paths {
                        let date = crate::sys::modified(&path);
                        out.push((path, date));
                    }
                    out
                })
                .await;
            this.update(cx, |this, cx| {
                // The user may have moved on to another folder already.
                if this.detail_meta_for.as_deref() != Some(key.as_str()) {
                    return;
                }
                if let Ok(mut map) = store.lock() {
                    map.clear();
                    for (path, date) in dates {
                        map.insert(
                            path,
                            DetailMeta {
                                stamp: date.as_ref().map(|(stamp, _)| *stamp),
                                label: date.map(|(_, label)| label),
                            },
                        );
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The open folder's children in the order the header asks for. Names are
    /// lower-cased once per row instead of once per comparison.
    fn detail_rows(
        &self,
        focus: &Arc<Node>,
        meta: Option<&HashMap<String, DetailMeta>>,
    ) -> Vec<(String, Arc<Node>)> {
        let mut rows: Vec<(String, Arc<Node>)> = focus
            .children
            .iter()
            .map(|child| (child.name.to_lowercase(), child.clone()))
            .collect();
        let sort = self.detail_sort;
        rows.sort_by(|(an, a), (bn, b)| {
            let by_name = an.cmp(bn);
            // Sorting by name or status keeps folders above files in both
            // directions, the way Explorer does; the column only flips the
            // names inside each group.
            let by_kind = (b.is_dir() as u8).cmp(&(a.is_dir() as u8));
            let ordering = match sort.key {
                DetailKey::Name | DetailKey::Status => {
                    by_kind.then(if sort.ascending { by_name } else { by_name.reverse() })
                }
                DetailKey::Size => a.size.cmp(&b.size),
                DetailKey::Files => a.files.cmp(&b.files),
                DetailKey::Folders => a.dirs.cmp(&b.dirs),
                DetailKey::Modified => {
                    let left = meta.and_then(|map| map.get(&a.path)).and_then(|item| item.stamp);
                    let right = meta.and_then(|map| map.get(&b.path)).and_then(|item| item.stamp);
                    left.cmp(&right).then(by_name)
                }
                DetailKey::Type => type_label(a).cmp(&type_label(b)).then(by_name),
            };
            match sort.key {
                DetailKey::Name | DetailKey::Status => ordering,
                _ if sort.ascending => ordering,
                _ => ordering.reverse(),
            }
        });
        rows
    }

    /// The Explorer-style details table: one row per child of the open folder,
    /// with the column header that drives the sort. The navigation pane beside
    /// it is laid out by the window, so every view puts it in the same place.
    fn details_area(&mut self, cx: &mut Context<Self>) -> Div {
        let Some(focus) = self.focus() else {
            return div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .justify_center()
                .child(self.scan_card(cx));
        };
        self.request_detail_meta(&focus, cx);

        let dates = self.detail_meta.lock().ok();
        let rows = self.detail_rows(&focus, dates.as_deref());
        self.detail_table(&focus, rows, dates.as_deref(), cx)
    }

    /// The left column: the volumes, then where the open folder sits in its own
    /// chain. Both are click targets, so the pane doubles as navigation — and it
    /// is the same pane in every view.
    fn detail_nav(
        &mut self,
        focus: &Arc<Node>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        // Whatever moved the chart — a click, a shortcut, a finished scan — the
        // tree opens the folders above it and follows along. Only on a change:
        // re-revealing every frame would re-open a folder the user just closed.
        let focus_path = focus.path.clone();
        if self.nav_revealed_for.as_deref() != Some(focus_path.as_str()) {
            self.nav_revealed_for = Some(focus_path.clone());
            self.reveal_in_nav(&focus_path);
        }
        // The bars are drawn from the measured viewport, which only exists after
        // the first layout: ask for one more frame when it changes.
        let size = self.nav_scroll.bounds().size;
        let measured = (f32::from(size.width), f32::from(size.height));
        if measured != self.nav_size {
            self.nav_size = measured;
            window.request_animation_frame();
        }

        // The rows inside the pane scroll both ways: a deep folder chain is
        // indented past the right edge, and a machine with several volumes runs
        // past the bottom. Neither should be cut off.
        let mut pane = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(NAV_PAD))
            .child(section_label("THIS PC"));

        // Volumes are the top level of the tree, and their folders hang off
        // them: one list instead of a drive picker above a folder list. The row
        // a click lands on is a node of this very tree, so it always names a
        // folder that exists — unlike a breadcrumb built from path text, which
        // can point at the middle of a chain the scanner folded into one node.
        let entries = self.nav_entries();
        let total = entries.len();
        for (index, entry) in entries.into_iter().enumerate() {
            match entry {
                NavEntry::Drive {
                    drive,
                    expanded,
                    has_children,
                    active,
                } => {
                    let fraction = usage_fraction(&drive);
                    let root = drive.root.clone();
                    let toggle = drive.root.clone();
                    let title = drive_title(&drive);
                    let usage = format!(
                        "{} free of {}",
                        theme::format_bytes(drive.free),
                        theme::format_bytes(drive.total)
                    );
                    let key = cache_key(&root);
                    pane = pane.child(
                        div()
                            .id(SharedString::from(format!("nav-drive-{key}")))
                            .flex()
                            .flex_col()
                            .gap(px(3.0))
                            .w(px(NAV_INNER_W))
                            .px(px(4.0))
                            .py(px(5.0))
                            .rounded(px(7.0))
                            .bg(if active {
                                theme::accent_soft()
                            } else {
                                theme::surface()
                            })
                            .hover(|style| style.bg(theme::surface_alt()))
                            // The header switches to the volume; the chevron
                            // only opens it, so both are separate targets.
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(3.0))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("nav-drive-toggle-{key}")))
                                            .w(px(12.0))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .text_size(px(8.5))
                                            .opacity(if has_children { 0.75 } else { 0.0 })
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.toggle_nav(&toggle, cx);
                                            }))
                                            .child(if expanded { "▾" } else { "▸" }),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!("nav-drive-open-{key}")))
                                            .flex_1()
                                            .min_w_0()
                                            .flex()
                                            .items_center()
                                            .gap(px(5.0))
                                            .cursor_pointer()
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if !this.last_root.eq_ignore_ascii_case(&root) {
                                                    this.switch_to(root.clone(), cx);
                                                }
                                            }))
                                            .child(div().text_size(px(11.0)).child("💾"))
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .truncate()
                                                    .child(title),
                                            ),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap(px(3.0))
                                    .pl(px(12.0))
                                    .child(
                                        div()
                                            .h(px(3.0))
                                            .w_full()
                                            .rounded_full()
                                            .bg(theme::track())
                                            .child(
                                                div()
                                                    .h_full()
                                                    .rounded_full()
                                                    .bg(if fraction > 0.9 {
                                                        theme::danger()
                                                    } else {
                                                        theme::accent()
                                                    })
                                                    // A fraction of the row rather
                                                    // than a fixed width. The row
                                                    // itself is pinned to the
                                                    // viewport, so the bar keeps
                                                    // one length while folders
                                                    // open and close.
                                                    .w(relative(fraction)),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(9.5))
                                            .text_color(theme::text_faint())
                                            .whitespace_nowrap()
                                            .child(usage),
                                    ),
                            ),
                    );
                }
                NavEntry::Folder(row) => {
                    let indent = 7.0 + row.depth as f32 * NAV_INDENT;
                    let key = cache_key(&row.node.path);
                    let is_focus = row.node.path.eq_ignore_ascii_case(&focus_path);
                    let target = row.node.path.clone();
                    let toggle = row.node.path.clone();
                    let name = if row.node.name.is_empty() {
                        row.node.path.clone()
                    } else {
                        row.node.name.clone()
                    };
                    pane = pane.child(
                        div()
                            .id(SharedString::from(format!("nav-row-{key}")))
                            .flex()
                            .items_center()
                            .h(px(NAV_ROW_H))
                            .pl(px(indent))
                            .pr(px(6.0))
                            .rounded(px(4.0))
                            .bg(if is_focus {
                                theme::accent_soft()
                            } else {
                                theme::surface()
                            })
                            .hover(|style| style.bg(theme::surface_alt()))
                            .child(
                                // The chevron opens and closes a folder without
                                // moving the chart, so it is its own click target
                                // rather than part of the row's.
                                div()
                                    .id(SharedString::from(format!("nav-toggle-{key}")))
                                    .w(px(12.0))
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(8.5))
                                    .opacity(if row.has_children { 0.75 } else { 0.0 })
                                    .cursor_pointer()
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.toggle_nav(&toggle, cx);
                                    }))
                                    .child(if row.expanded { "▾" } else { "▸" }),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("nav-open-{key}")))
                                    .flex_1()
                                    .min_w_0()
                                    .h_full()
                                    .flex()
                                    .items_center()
                                    .gap(px(4.0))
                                    .cursor_pointer()
                                    .font_weight(if is_focus {
                                        FontWeight::MEDIUM
                                    } else {
                                        FontWeight::NORMAL
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.jump_to(target.clone(), cx);
                                    }))
                                    .child(div().flex_shrink_0().whitespace_nowrap().child(name))
                                    .child(
                                        div()
                                            .text_size(px(9.0))
                                            .text_color(theme::text_faint())
                                            .whitespace_nowrap()
                                            .child(theme::format_bytes(row.node.size)),
                                    ),
                            ),
                    );
                }
            }
            if index + 1 == total && total >= MAX_NAV_ROWS {
                pane = pane.child(
                    div()
                        .pl(px(19.0))
                        .py(px(3.0))
                        .text_size(px(9.0))
                        .text_color(theme::text_faint())
                        .child("Close a folder to see more"),
                );
            }
        }

        // The frame stays the width the layout reserves; only its contents move.
        let vertical = self.nav_bar(true);
        let horizontal = self.nav_bar(false);
        let mut frame = div()
            .w(px(NAV_W))
            .flex_shrink_0()
            .relative()
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .text_size(px(11.5))
            .overflow_hidden()
            // A drag that started on a thumb follows the pointer anywhere over
            // the pane, so the move and release are handled by the frame.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_nav(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &MouseUpEvent, _, cx| this.end_nav_drag(cx)),
            )
            .child(
                // A row, with the content sized to what it needs rather than
                // stretched: the scroll extent is measured from this child's
                // own box, so it has to be allowed to grow past the frame.
                div()
                    .id("nav-scroll")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .items_start()
                    .track_scroll(&self.nav_scroll)
                    .overflow_scroll()
                    .child(pane.flex_shrink_0().min_w(px(NAV_W - 2.0))),
            );

        // The window backend paints no scrollbars of its own, so the pane draws
        // its own from the handle's offset: both axes are usable without a
        // trackpad or a Shift+wheel.
        if let Some((thumb, track)) = vertical {
            let top = (track - thumb) * self.nav_progress(true);
            frame = frame.child(
                div()
                    .absolute()
                    .top(px(NAV_BAR))
                    .right(px(1.0))
                    .h(px(track))
                    .w(px(NAV_BAR))
                    .rounded_full()
                    .bg(theme::track())
                    .child(
                        div()
                            .absolute()
                            .top(px(top))
                            .w_full()
                            .h(px(thumb))
                            .rounded_full()
                            .bg(theme::text_faint())
                            .hover(|style| style.bg(theme::text_dim()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.begin_nav_drag(true, event, cx);
                                }),
                            ),
                    ),
            );
        }
        if let Some((thumb, track)) = horizontal {
            let left = (track - thumb) * self.nav_progress(false);
            frame = frame.child(
                div()
                    .absolute()
                    .left(px(NAV_BAR))
                    .bottom(px(1.0))
                    .w(px(track))
                    .h(px(NAV_BAR))
                    .rounded_full()
                    .bg(theme::track())
                    .child(
                        div()
                            .absolute()
                            .left(px(left))
                            .h_full()
                            .w(px(thumb))
                            .rounded_full()
                            .bg(theme::text_faint())
                            .hover(|style| style.bg(theme::text_dim()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.begin_nav_drag(false, event, cx);
                                }),
                            ),
                    ),
            );
        }
        frame
    }

    /// The table itself: the folder's own toolbar, the column header, the
    /// scrollable rows and a summary line.
    fn detail_table(
        &self,
        focus: &Arc<Node>,
        rows: Vec<(String, Arc<Node>)>,
        meta: Option<&HashMap<String, DetailMeta>>,
        cx: &mut Context<Self>,
    ) -> Div {
        let folders = rows.iter().filter(|(_, node)| node.is_dir()).count();
        let files = rows.len() - folders;
        let total: u64 = rows.iter().map(|(_, node)| node.size).sum();

        let mut body = div()
            .id("detail-rows")
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .track_scroll(&self.detail_scroll)
            .overflow_y_scroll();
        for (index, (_, node)) in rows.into_iter().enumerate() {
            let selected = self
                .detail_selected
                .as_ref()
                .is_some_and(|path| path.eq_ignore_ascii_case(&node.path));
            body = body.child(self.detail_row(index, node, selected, meta, cx));
        }
        if focus.children.is_empty() {
            body = body.child(
                div()
                    .px(px(12.0))
                    .py(px(14.0))
                    .text_size(px(11.0))
                    .text_color(theme::text_faint())
                    .child("This folder is empty."),
            );
        }

        // The window backend paints no scrollbars of its own, so the table draws
        // one from the handle's offset, the way the navigation pane does. It is
        // pinned to the right edge of the rows rather than the card, so it never
        // covers the toolbar or the footer.
        let mut rows = div()
            .relative()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(body);
        if let Some((thumb, track)) = self.detail_bar() {
            let top = (track - thumb) * self.detail_progress();
            rows = rows.child(
                div()
                    .absolute()
                    .top(px(DETAIL_BAR))
                    .right(px(1.0))
                    .h(px(track))
                    .w(px(DETAIL_BAR))
                    .rounded_full()
                    .bg(theme::track())
                    .child(
                        div()
                            .absolute()
                            .top(px(top))
                            .w_full()
                            .h(px(thumb))
                            .rounded_full()
                            .bg(theme::text_faint())
                            .hover(|style| style.bg(theme::text_dim()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                                    this.begin_detail_drag(event, cx);
                                }),
                            ),
                    ),
            );
        }

        div()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .overflow_hidden()
            // A scrollbar or column drag that started inside the table follows
            // the pointer wherever it goes, so the card handles the moves.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.drag_detail(event, cx);
                this.drag_columns(event, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.end_detail_drag(cx);
                    this.end_col_drag(cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(8.0))
                    .py(px(5.0))
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme::hairline())
                    .child(self.ghost_button("↑ Up", true, cx, |this, cx| this.go_up(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(self.path_crumbs(&focus.path, cx)),
                    )
                    .child(self.action_button("↗ Reveal", cx, |this, _| {
                        if let Some(node) = this.selected_detail_node() {
                            crate::sys::reveal(&node.path, node.is_dir());
                        }
                    }))
                    .child(self.danger_button(
                        "🗑 Delete…",
                        self.selected_detail_node().is_some(),
                        cx,
                        |this, cx| {
                            if let Some(node) = this.selected_detail_node() {
                                this.ask_delete(node, cx);
                            }
                        },
                    )),
            )
            .child(self.detail_header(cx))
            .child(rows)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(10.0))
                    .py(px(4.0))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme::hairline())
                    .text_size(px(10.0))
                    .text_color(theme::text_faint())
                    .child(format!(
                        "{} · {}",
                        count_label(folders, "folder", "folders"),
                        count_label(files, "file", "files")
                    ))
                    .child(format!("{} total", theme::format_bytes(total))),
            )
    }

    fn detail_header(&self, cx: &mut Context<Self>) -> Div {
        div()
            .flex()
            .items_center()
            .h(px(DETAIL_HEADER_H))
            .px(px(4.0))
            .flex_shrink_0()
            .bg(theme::surface_alt())
            .border_b_1()
            .border_color(theme::hairline())
            .child(self.detail_column("Name", DetailKey::Name, false, cx))
            .child(self.detail_column("Size", DetailKey::Size, true, cx))
            .child(self.detail_column("Files", DetailKey::Files, true, cx))
            .child(self.detail_column("Folders", DetailKey::Folders, true, cx))
            .child(self.detail_column("Status", DetailKey::Status, false, cx))
            .child(self.detail_column(
                "Date Modified",
                DetailKey::Modified,
                false,
                cx,
            ))
            .child(self.detail_column("Type", DetailKey::Type, false, cx))
            // Any room the columns leave over stays on the right, so a wider
            // window never stretches a column the user sized by hand.
            .child(div().flex_1())
    }

    /// One fixed-width header cell: the sortable label, plus the grip on its
    /// right edge that widens or narrows the column.
    fn detail_column(
        &self,
        label: &str,
        key: DetailKey,
        right: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let width = self.columns.of(key);
        div()
            .relative()
            .w(px(width))
            .flex_basis(px(width))
            .h(px(DETAIL_HEADER_H))
            .flex_shrink()
            .child(
                self.detail_header_cell(label, key, right, cx)
                    .w_full()
                    .h_full(),
            )
            .child(
                div()
                    .id(SharedString::from(format!("detail-grip-{label}")))
                    .absolute()
                    .top_0()
                    .right(px(-COL_GRIP))
                    .h_full()
                    .w(px(COL_GRIP))
                    .rounded(px(2.0))
                    .cursor(CursorStyle::ResizeLeftRight)
                    .hover(|style| style.bg(theme::hairline()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            this.begin_col_drag(key, event, cx);
                        }),
                    ),
            )
    }

    fn detail_header_cell(
        &self,
        label: &str,
        key: DetailKey,
        right: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let active = self.detail_sort.key == key;
        let arrow = if active {
            if self.detail_sort.ascending {
                "▲"
            } else {
                "▼"
            }
        } else {
            ""
        };
        let mut cell = div()
            .id(SharedString::from(format!("detail-sort-{label}")))
            .flex()
            .items_center()
            .gap(px(3.0))
            .px(px(6.0))
            .py(px(4.0))
            .rounded(px(4.0))
            .cursor_pointer()
            .text_size(px(10.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(if active {
                theme::text()
            } else {
                theme::text_dim()
            })
            .hover(|style| style.bg(theme::surface()).text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| this.set_detail_sort(key, cx)))
            .child(label.to_string());
        if right {
            cell = cell.justify_end();
        }
        cell.child(
            div()
                .text_size(px(7.0))
                .text_color(theme::accent())
                .child(arrow.to_string()),
        )
    }

    fn detail_row(
        &self,
        index: usize,
        node: Arc<Node>,
        selected: bool,
        meta: Option<&HashMap<String, DetailMeta>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let click = node.clone();
        let menu_path = node.path.clone();
        let glyph = match node.kind {
            Kind::Dir => "📁",
            Kind::File => "📄",
            Kind::Other => "⋯",
        };
        // An aggregated "(n more)" row has no size of its own to report.
        let size = if node.kind == Kind::Other {
            "—".to_string()
        } else {
            theme::format_bytes(node.size)
        };
        // Folders report the state of their walk; a file has nothing to say.
        let status = if node.is_dir() { "OK" } else { "" };
        let date = meta
            .and_then(|map| map.get(&node.path))
            .and_then(|item| item.label.clone())
            .unwrap_or_else(|| "—".to_string());
        let kind = type_label(&node);
        let dim = node.kind == Kind::Other;

        div()
            .id(SharedString::from(format!("detail-row-{index}")))
            .flex()
            .items_center()
            .h(px(DETAIL_ROW_H))
            .px(px(4.0))
            .flex_shrink_0()
            .cursor_pointer()
            .bg(if selected {
                theme::accent_soft()
            } else {
                theme::surface()
            })
            .hover(|style| style.bg(theme::surface_alt()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.detail_click(click.clone(), cx);
            }))
            // The right button asks the shell for its own menu — the same one
            // Explorer shows — and takes the row's selection with it.
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    this.detail_selected = Some(menu_path.clone());
                    cx.notify();
                    this.shell_menu(menu_path.clone(), cx);
                }),
            )
            .child(
                div()
                    .w(px(self.columns.of(DetailKey::Name)))
                    .flex_shrink()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(6.0))
                    .child(div().text_size(px(11.0)).child(glyph))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(if dim {
                                theme::text_dim()
                            } else {
                                theme::text()
                            })
                            .child(node.name.clone()),
                    ),
            )
            .child(detail_cell(
                size,
                self.columns.of(DetailKey::Size),
                true,
            ))
            .child(detail_cell(
                theme::format_count(node.files),
                self.columns.of(DetailKey::Files),
                true,
            ))
            .child(detail_cell(
                theme::format_count(node.dirs),
                self.columns.of(DetailKey::Folders),
                true,
            ))
            .child(detail_cell(
                status.to_string(),
                self.columns.of(DetailKey::Status),
                false,
            ))
            .child(detail_cell(
                date,
                self.columns.of(DetailKey::Modified),
                false,
            ))
            .child(detail_cell(kind, self.columns.of(DetailKey::Type), false))
            .child(div().flex_1())
            .into_any_element()
    }

    // ---------------------------------------------------------------- render

    fn top_bar(&self, cx: &mut Context<Self>) -> Div {
        let mut bar = div()
            .h(px(TOPBAR_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap(px(12.0))
            .px(px(PAD + 4.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .child(
                        div()
                            .text_size(px(19.0))
                            .text_color(theme::accent())
                            .child("◔"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_size(px(14.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Disk Scanner"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_faint())
                                    .child("sunburst disk usage"),
                            ),
                    ),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(self.view_button(View::Rings, cx))
                    .child(self.view_button(View::Treemap, cx))
                    .child(self.view_button(View::Heatmap, cx))
                    .child(self.view_button(View::Details, cx))
                    .child(self.view_button(View::Files, cx))
                    .child(self.view_button(View::Types, cx)),
            );
        // The metric toggle re-proportions a chart. The details table, the file
        // list and the type totals each carry their own numbers, so the buttons
        // mean nothing while one of those is open.
        if self.view.is_chart() {
            bar = bar.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(self.metric_button("Size", Metric::Bytes, cx))
                    .child(self.metric_button("Files", Metric::Files, cx)),
            );
        }
        bar.child(self.theme_button(cx))
    }

    /// The Rescan button says what it will actually do: refresh the folder in
    /// the focus card once you have zoomed in, otherwise re-walk the volume.
    fn rescan_label(&self) -> &'static str {
        let inside = !self.focus_path.is_empty()
            && !self.focus_path.eq_ignore_ascii_case(&self.last_root)
            && self.tree.is_some();
        if inside {
            "Rescan folder"
        } else {
            "Rescan"
        }
    }

    fn theme_button(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let mode = self.theme_mode;
        let label = format!("◐ {}", mode.label());
        div()
            .id("theme-toggle")
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .text_color(theme::text_dim())
            .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.set_theme_mode(mode.next(), cx);
            }))
            .child(label)
    }

    /// The active volume, if it is still plugged in. A scan can be rooted at a
    /// folder rather than at `E:\`, so an exact match is tried first and then
    /// the drive whose root contains the open path.
    fn active_drive(&self) -> Option<&DriveInfo> {
        self.drives.iter().find(|drive| {
            self.last_root.eq_ignore_ascii_case(&drive.root)
                || is_inside(&drive.root, &self.last_root)
        })
    }

    fn view_button(&self, view: View, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let active = self.view == view;
        let glyph = match view {
            View::Rings => "◎",
            View::Treemap => "▦",
            View::Heatmap => "▩",
            View::Details => "☰",
            View::Files => "≡",
            View::Types => "◆",
        };
        div()
            .id(SharedString::from(format!("view-{glyph}")))
            .flex()
            .items_center()
            .gap(px(4.0))
            .px(px(8.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .bg(if active {
                theme::accent_soft()
            } else {
                theme::surface()
            })
            .text_color(if active {
                theme::text()
            } else {
                theme::text_dim()
            })
            .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| this.set_view(view, cx)))
            .child(
                div()
                    .text_size(px(12.0))
                    .text_color(if active {
                        theme::accent()
                    } else {
                        theme::text_faint()
                    })
                    .child(glyph),
            )
            .child(view.label().to_string())
    }

    fn metric_button(&self, label: &str, metric: Metric, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let active = self.metric == metric;
        div()
            .id(SharedString::from(format!("metric-{label}")))
            .px(px(10.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .bg(if active {
                theme::accent_soft()
            } else {
                theme::surface()
            })
            .text_color(if active {
                theme::text()
            } else {
                theme::text_dim()
            })
            .hover(|style| style.bg(theme::surface_alt()))
            .on_click(cx.listener(move |this, _, _, cx| this.set_metric(metric, cx)))
            .child(label.to_string())
    }

    fn action_button(
        &self,
        label: &str,
        cx: &mut Context<Self>,
        action: impl Fn(&mut DiskApp, &mut Context<DiskApp>) + 'static,
    ) -> gpui::Stateful<Div> {
        div()
            .id(SharedString::from(format!("action-{label}")))
            .px(px(11.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .text_size(px(11.5))
            .text_color(theme::text_dim())
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
            .child(label.to_string())
    }

    /// A borderless button for the focus card: navigation and secondary folder
    /// actions belong beside the path they act on, not in the global top bar.
    fn ghost_button(
        &self,
        label: &str,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut DiskApp, &mut Context<DiskApp>) + 'static,
    ) -> gpui::Stateful<Div> {
        let button = div()
            .id(SharedString::from(format!("ghost-{label}")))
            .px(px(7.0))
            .py(px(3.0))
            .rounded(px(6.0))
            .text_size(px(11.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme::text_dim())
            .child(label.to_string());
        if !enabled {
            return button.opacity(0.34);
        }
        button
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
    }

    /// Same shape as `action_button` but tinted for a destructive action.
    fn danger_button(
        &self,
        label: &str,
        enabled: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut DiskApp, &mut Context<DiskApp>) + 'static,
    ) -> gpui::Stateful<Div> {
        let button = div()
            .id(SharedString::from(format!("danger-{label}")))
            .px(px(9.0))
            .py(px(5.0))
            .rounded(px(7.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .text_size(px(11.5))
            .text_color(theme::danger())
            .child(label.to_string());
        if !enabled {
            return button.opacity(0.38);
        }
        button
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_alt()))
            .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
    }

    fn chart_area(
        &mut self,
        viewport_w: f32,
        viewport_h: f32,
        nav_w: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The navigation pane is a sibling, so its width is not part of the
        // chart's share; `PANEL_W` only exists in the chart views.
        let content_w = viewport_w - PAD * 2.0 - nav_w;
        let content_h = viewport_h - TOPBAR_H - STATUSBAR_H - PAD * 2.0;
        let available_w = content_w - PANEL_W - GAP;
        let side = available_w.min(content_h).max(240.0) - 4.0;

        let scanning = matches!(self.phase, Phase::Scanning);

        if self.view == View::Details {
            return self.details_area(cx).into_any_element();
        }
        if self.view == View::Files {
            return self.files_area(cx).into_any_element();
        }
        if self.view == View::Types {
            return self.types_area(cx).into_any_element();
        }

        let Some(chart) = self.chart.clone() else {
            return div()
                .relative()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .justify_center()
                .child(self.scan_card(cx))
                .into_any_element()
        };

        if self.view != View::Rings {
            return self.tiles_area(&chart, scanning, window, cx).into_any_element();
        }

        let rings = chart.rings;
        let geom = Geom::new(side, rings);
        let placed = self.placed.clone();
        if let Ok(mut slot) = placed.lock() {
            // Claim the layout for the sunburst. The tile canvas is the only
            // other writer, and without this line a treemap frame left behind
            // would keep serving hit tests for a picture that is not on screen.
            slot.geom = geom;
            slot.view = View::Rings;
            slot.chart = Some(chart.clone());
        }

        let hover = self.hover;
        let chain = hover.map(|index| ancestors(&chart, index)).unwrap_or_default();

        let mut labels: Vec<AnyElement> = Vec::new();
        // While the context menu is up its own labels are the only text worth
        // showing; slice names would bleed through the card.
        for (index, slice) in chart.slices.iter().enumerate() {
            if self.menu.is_some() || slice.depth > 3 {
                continue;
            }
            let (r0, r1) = geom.ring_bounds(slice.depth);
            if r1 - r0 < 16.0 {
                continue;
            }
            let arc_len = slice.span() * (r0 + r1) * 0.5;
            let text = short_name(&slice.node.name, 16);
            let estimate = text.chars().count() as f32 * 6.1 + 10.0;
            if arc_len < estimate {
                continue;
            }
            let mid = slice.mid();
            let radius = (r0 + r1) * 0.5;
            let x = geom.cx + radius * mid.cos();
            let y = geom.cy + radius * mid.sin();
            let box_w = estimate;
            let dimmed = hover.is_some() && !chain.contains(&index);
            labels.push(
                div()
                    .absolute()
                    .left(px(x - box_w * 0.5))
                    .top(px(y - 8.0))
                    .w(px(box_w))
                    .text_size(px(9.5))
                    .text_color(if dimmed {
                        theme::text_faint()
                    } else {
                        theme::text()
                    })
                    .opacity(if dimmed { 0.55 } else { 0.92 })
                    .text_center()
                    .child(text)
                    .into_any_element(),
            );
            if labels.len() >= 48 {
                break;
            }
        }

        if let Some(menu) = &self.menu {
            let origin = self
                .placed
                .lock()
                .map(|slot| slot.origin)
                .unwrap_or_else(|_| point(px(0.), px(0.)));
            let delete_label = if menu.node.is_dir() {
                "Delete folder…"
            } else {
                "Delete file…"
            };
            labels.push(menu_label(
                menu,
                origin,
                9.0,
                short_name(&menu.node.name, 30),
                theme::text_faint(),
                FontWeight::NORMAL,
            ));
            labels.push(menu_label(
                menu,
                origin,
                MENU_REVEAL_TOP + 7.0,
                "Reveal in Explorer".to_string(),
                theme::text(),
                FontWeight::NORMAL,
            ));
            labels.push(menu_label(
                menu,
                origin,
                MENU_DELETE_TOP + 7.0,
                delete_label.to_string(),
                theme::danger(),
                FontWeight::NORMAL,
            ));
        }

        let hub_name = short_name(&chart.focus.name, 18);
        let hub_value = match self.metric {
            Metric::Bytes => theme::format_bytes(chart.focus.size),
            Metric::Files => theme::format_count(chart.focus.files),
        };
        let hub_caption = if chart.rings <= 1 && chart.slices.is_empty() {
            "empty folder".to_string()
        } else if chart.rings == 1 {
            format!("{} · 1 ring", self.metric.unit())
        } else {
            format!("{} · {} rings", self.metric.unit(), chart.rings)
        };

        let side_px = side;
        let rings = chart.rings;
        let store_prepaint = placed.clone();
        let chart_prepaint = chart.clone();
        let chart_paint = chart.clone();
        let store_paint = placed.clone();
        let menu_paint = self.menu.clone();
        let menu_hover = self.menu_hover;
        let mut area = div()
            // The id is what lets the column own hover state: `on_hover` is only
            // available on stateful elements.
            .id("chart-area")
            .relative()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .relative()
                    .w(px(side_px))
                    .h(px(side_px))
                    .child(
                        canvas(
                            move |bounds, _, _| {
                                let side =
                                    f32::from(bounds.size.width).min(f32::from(bounds.size.height));

                                let geom = Geom::new(side, rings);
                                if let Ok(mut slot) = store_prepaint.lock() {
                                    slot.origin = bounds.origin;
                                    slot.geom = geom;
                                    slot.view = View::Rings;
                                    slot.chart = Some(chart_prepaint.clone());
                                }
                                geom
                            },
                            move |_, geom, window, _| {
                                let origin = store_paint
                                    .lock()
                                    .map(|slot| slot.origin)
                                    .unwrap_or_else(|_| point(px(0.), px(0.)));
                                paint_sunburst(window, origin, &geom, &chart_paint, hover, &chain);
                                if let Some(menu) = &menu_paint {
                                    paint_menu(window, origin, menu, menu_hover);
                                }
                            },
                        )
                        .size_full(),
                    )
                    .children(labels)
                    .child(
                        div()
                            .absolute()
                            .left(px(geom.cx - 100.0))
                            .top(px(geom.cy - 30.0))
                            .w(px(200.0))
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap(px(1.0))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme::text_dim())
                                    .child(hub_name),
                            )
                            .child(
                                div()
                                    .text_size(px(21.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child(hub_value),
                            )
                            .child(
                                div()
                                    .text_size(px(9.0))
                                    .text_color(theme::text_faint())
                                    .child(hub_caption),
                            ),
                    ),
            )
            // The handlers live on the whole column, not just the square: the
            // volume list hangs below the top bar and must swallow its clicks
            // even where it reaches past the chart.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                this.on_chart_move(event, cx);
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                // A highlight must not outlive the pointer that caused it: with
                // the cursor in the panel, the ring would keep one folder lit
                // and leave its tooltip hanging in mid-air.
                if !*hovered && (this.hover.is_some() || this.tooltip.is_some()) {
                    this.clear_hover();
                    cx.notify();
                }
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.on_chart_click(event, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.on_chart_right_click(event, window, cx);
                }),
            );
        // While a *different* volume is being walked, the chart on screen is
        // yesterday's news: veil it so nobody reads C: numbers as G: numbers.
        if scanning && !chart.focus.path.eq_ignore_ascii_case(&self.last_root) {
            area = area.child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .w_full()
                    .h_full()
                    .bg(theme::scrim()),
            );
        }
        if scanning {
            area = area.child(self.scan_pill(cx));
        } else if matches!(self.phase, Phase::Stopped) {
            area = area.child(self.stopped_pill(cx));
        }
        area.into_any_element()
    }

    /// Treemap and heatmap share one canvas: rectangles from the model painted
    /// as quads, labels floated on top, and the same hover / click / menu
    /// handling as the sunburst.
    fn tiles_area(
        &mut self,
        chart: &Arc<ChartModel>,
        scanning: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let view = self.view;
        let hover = self.hover;
        let chain = hover.map(|index| ancestors(chart, index)).unwrap_or_default();

        // The canvas measures itself during prepaint; the labels for this
        // frame are laid out from the size measured a moment earlier, so they
        // line up with the cells painted underneath them. Prepaint already
        // laid those cells out for this very chart and geometry, so the labels
        // reuse them rather than running squarify a second time every frame.
        let mut measured = (0.0, 0.0);
        let mut cached: Option<Arc<Vec<Tile>>> = None;
        if let Ok(slot) = self.placed.lock() {
            measured = slot.size;
            if measured.0 > 1.0
                && measured.1 > 1.0
                && !slot.tiles.is_empty()
                && slot.describes(view, chart)
            {
                cached = Some(slot.tiles.clone());
            }
        }
        let tiles: Arc<Vec<Tile>> = match cached {
            Some(tiles) => tiles,
            None if measured.0 > 1.0 && measured.1 > 1.0 => Arc::new(match view {
                View::Heatmap => heatmap(chart, measured.0, measured.1),
                _ => treemap(chart, measured.0, measured.1),
            }),
            None => {
                if measured != self.tile_size {
                    self.tile_size = measured;
                    window.request_animation_frame();
                }
                Arc::new(Vec::new())
            }
        };

        let mut labels: Vec<AnyElement> = Vec::new();
        // Parents keep a name strip at the top of their cell, so their label
        // never collides with a child's; small parents get no label at all.
        let mut has_children = vec![false; chart.slices.len()];
        for slice in &chart.slices {
            if slice.parent != usize::MAX {
                has_children[slice.parent] = true;
            }
        }
        let (min_w, min_h) = if view == View::Heatmap {
            (36.0, 12.0)
        } else {
            (58.0, 16.0)
        };
        let mut ordered: Vec<usize> = (0..tiles.len().min(chart.slices.len())).collect();
        ordered.sort_by(|&a, &b| tiles[b].area().total_cmp(&tiles[a].area()));
        for &index in &ordered {
            let tile = tiles[index];
            let slice = &chart.slices[index];
            if tile.w < min_w || tile.h < min_h {
                continue;
            }
            if view != View::Heatmap
                && has_children[index]
                && (tile.h < TREEMAP_HEADER + 22.0 || tile.w < 40.0)
            {
                continue;
            }
            let dimmed = view != View::Heatmap && hover.is_some() && !chain.contains(&index);
            let max_chars = ((tile.w - 6.0) / 5.4).clamp(3.0, 40.0) as usize;
            labels.push(
                div()
                    .absolute()
                    .left(px(tile.x + 4.0))
                    .top(px(tile.y + 2.0))
                    .w(px(tile.w - 8.0))
                    .overflow_hidden()
                    .text_size(px(9.5))
                    .text_color(if dimmed {
                        theme::text_faint()
                    } else {
                        theme::text()
                    })
                    .opacity(if dimmed { 0.55 } else { 0.9 })
                    .child(short_name(&slice.node.name, max_chars))
                    .into_any_element(),
            );
            if labels.len() >= 64 {
                break;
            }
        }


        let chart_prepaint = chart.clone();
        let chart_paint = chart.clone();
        let store_prepaint = self.placed.clone();
        let paint_chain = chain.clone();
        let menu_paint_tiles = self.menu.clone();
        let menu_hover_tiles = self.menu_hover;

        if let Some(menu) = &self.menu {
            let origin = self
                .placed
                .lock()
                .map(|slot| slot.origin)
                .unwrap_or_else(|_| point(px(0.), px(0.)));
            let delete_label = if menu.node.is_dir() {
                "Delete folder…"
            } else {
                "Delete file…"
            };
            labels.push(menu_label(
                menu,
                origin,
                9.0,
                short_name(&menu.node.name, 30),
                theme::text_faint(),
                FontWeight::NORMAL,
            ));
            labels.push(menu_label(
                menu,
                origin,
                MENU_REVEAL_TOP + 7.0,
                "Reveal in Explorer".to_string(),
                theme::text(),
                FontWeight::NORMAL,
            ));
            labels.push(menu_label(
                menu,
                origin,
                MENU_DELETE_TOP + 7.0,
                delete_label.to_string(),
                theme::danger(),
                FontWeight::NORMAL,
            ));
        }

        let mut area = div().relative().flex_1().min_w_0().child(
            div()
                .relative()
                .w_full()
                .h_full()
                .child(
                    canvas(
                        move |bounds, _, _| {
                            let w = f32::from(bounds.size.width).max(1.0);
                            let h = f32::from(bounds.size.height).max(1.0);
                            let tiles = Arc::new(match view {
                                View::Heatmap => heatmap(&chart_prepaint, w, h),
                                _ => treemap(&chart_prepaint, w, h),
                            });
                            if let Ok(mut slot) = store_prepaint.lock() {
                                slot.origin = bounds.origin;
                                slot.tiles = tiles.clone();
                                slot.view = view;
                                slot.chart = Some(chart_prepaint.clone());
                                slot.size = (w, h);
                            }
                            tiles
                        },
                        move |bounds, tiles, window, _| {
                            paint_tiles(
                                window,
                                bounds.origin,
                                &tiles,
                                &chart_paint,
                                view,
                                hover,
                                &paint_chain,
                            );
                            if let Some(menu) = &menu_paint_tiles {
                                paint_menu(window, bounds.origin, menu, menu_hover_tiles);
                            }
                        },
                    )
                    .size_full(),
                )
                .children(labels)
                .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                    this.on_chart_move(event, cx);
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, _, cx| {
                        this.on_chart_click(event, cx);
                    }),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.on_chart_right_click(event, window, cx);
                    }),
                ),
        );

        if scanning && !chart.focus.path.eq_ignore_ascii_case(&self.last_root) {
            area = area.child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .w_full()
                    .h_full()
                    .bg(theme::scrim()),
            );
        }
        if scanning {
            area = area.child(self.scan_pill(cx));
        } else if matches!(self.phase, Phase::Stopped) {
            area = area.child(self.stopped_pill(cx));
        }
        area
    }

    /// The floating progress pill shown over a chart while its numbers are
    /// being refreshed.
    fn scan_pill(&self, cx: &mut Context<Self>) -> Div {
        let (files, bytes, path) = &self.progress;
        div()
            .absolute()
            .top(px(8.0))
            .left(px(0.0))
            .right(px(0.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .pl(px(13.0))
                    .pr(px(5.0))
                    .py(px(5.0))
                    .max_w(px(560.0))
                    .rounded_full()
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::hairline())
                    .text_size(px(11.0))
                    .text_color(theme::text_dim())
                    .child(div().text_color(theme::accent()).child("●"))
                    .child(
                        div().truncate().child(format!(
                            "Scanning {} · {} files · {}",
                            short_name(path, 38),
                            theme::format_count(*files),
                            theme::format_bytes(*bytes)
                        )),
                    )
                    .child(self.action_button("Stop", cx, |this, cx| this.stop_scan(cx))),
            )
    }

    /// The pill a stopped scan leaves over the chart it was refreshing. The
    /// chart keeps the cached numbers, and Continue lets the parked walk go on.
    fn stopped_pill(&self, cx: &mut Context<Self>) -> Div {
        div()
            .absolute()
            .top(px(8.0))
            .left(px(0.0))
            .right(px(0.0))
            .flex()
            .justify_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .pl(px(13.0))
                    .pr(px(5.0))
                    .py(px(5.0))
                    .rounded_full()
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::hairline())
                    .text_size(px(11.0))
                    .text_color(theme::text_dim())
                    .child(div().text_color(theme::text_faint()).child("●"))
                    .child("Scan stopped")
                    .child(self.action_button("Continue", cx, |this, cx| this.continue_scan(cx))),
            )
    }

    /// The full-card placeholder used before any scan has produced a chart.
    fn scan_card(&self, cx: &mut Context<Self>) -> Div {
        let (files, bytes, path) = &self.progress;
        let scanning = matches!(self.phase, Phase::Scanning);
        let stopped = matches!(self.phase, Phase::Stopped);
        let title = match &self.phase {
            Phase::Scanning => format!("Scanning {}…", self.last_root),
            Phase::Stopped => "Scan stopped".to_string(),
            Phase::Failed(error) => format!("Could not scan: {error}"),
            Phase::Ready { .. } => "Waiting for the first scan…".to_string(),
            Phase::Empty => "Waiting for a drive…".to_string(),
        };
        let mut card = div()
            .w(px(440.0))
            .p(px(20.0))
            .rounded(px(12.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .flex()
            .flex_col()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(title),
            );
        if scanning || stopped {
            // A stopped walk has no chart yet, so this card is the only place
            // the progress it reached is visible.
            card = card
                .child(
                    div()
                        .text_size(px(11.0))
                        .text_color(theme::text_dim())
                        .child(format!(
                            "{} files · {}",
                            theme::format_count(*files),
                            theme::format_bytes(*bytes)
                        )),
                )
                .child(
                    div()
                        .text_size(px(10.0))
                        .text_color(theme::text_faint())
                        .truncate()
                        .child(short_name(path, 60)),
                );
        }
        if scanning {
            card = card.child(self.action_button("Stop", cx, |this, cx| this.stop_scan(cx)));
        } else if stopped {
            card = card.child(self.action_button("Continue", cx, |this, cx| this.continue_scan(cx)));
        }
        card
    }

    fn pick(&self, position: Point<Pixels>) -> Option<(usize, f32, f32)> {
        let chart = self.chart.as_ref()?;
        let slot = self.placed.lock().ok()?;
        // The stored layout is whatever the last painted canvas measured: after
        // a switch between rings and the tile views, or a new chart, those
        // rectangles belong to a different picture. Hitting them anyway maps
        // the pointer onto an unrelated slice, so the highlight, the tooltip
        // and the folder a click opens would all name the wrong thing.
        if !slot.describes(self.view, chart) {
            return None;
        }
        let local_x = f32::from(position.x - slot.origin.x);
        let local_y = f32::from(position.y - slot.origin.y);
        let index = match slot.view {
            View::Rings => hit(chart, &slot.geom, local_x, local_y),
            // Nested treemap cells overlap: the smallest one under the cursor
            // is the deepest folder, which is the one the user means.
            _ => slot
                .tiles
                .iter()
                .enumerate()
                .filter(|(_, tile)| tile.contains(local_x, local_y))
                .min_by(|a, b| a.1.area().total_cmp(&b.1.area()))
                .map(|(index, _)| index),
        }?;
        if index >= chart.slices.len() {
            return None;
        }
        Some((index, f32::from(position.x), f32::from(position.y)))
    }

    /// Highlight a slice from the breakdown list, so the ring and the list agree
    /// on which folder is "the one under the pointer". No tooltip: the card
    /// follows the pointer over the chart, and from the list it would sit on top
    /// of the neighbouring rows while the row itself already shows the numbers.
    fn hover_folder(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.hover == Some(index) && self.tooltip.is_none() {
            return;
        }
        self.hover = Some(index);
        self.tooltip = None;
        cx.notify();
    }

    fn on_chart_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
        // The menu takes over the pointer: rows light up, the chart stops
        // tracking hover so its tooltip does not fight the menu for space.
        if let Some(menu) = &self.menu {
            let hover = menu_row_at(menu, f32::from(event.position.x), f32::from(event.position.y));
            if hover != self.menu_hover {
                self.menu_hover = hover;
                if self.hover.is_some() || self.tooltip.is_some() {
                    self.hover = None;
                    self.tooltip = None;
                }
                cx.notify();
            }
            return;
        }
        let picked = self.pick(event.position);
        let next = picked.map(|(index, _, _)| index);
        if next == self.hover && self.tooltip.is_some() {
            return;
        }
        self.hover = next;
        self.tooltip = picked.map(|(index, x, y)| Tooltip { x, y, index });
        cx.notify();
    }

    fn on_chart_click(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        // A press on an open menu resolves against its rows; a press anywhere
        // else dismisses it. Either way the chart underneath stays put.
        if let Some(menu) = self.menu.clone() {
            let x = f32::from(event.position.x);
            let y = f32::from(event.position.y);
            let row = menu_row_at(&menu, x, y);
            let inside = menu_contains(&menu, x, y);
            self.menu = None;
            self.menu_hover = None;
            match row {
                Some(0) => crate::sys::reveal(&menu.node.path, menu.node.is_dir()),
                Some(1) => self.ask_delete(menu.node, cx),
                _ => {}
            }
            cx.notify();
            if inside {
                return;
            }
        }
        let Some(chart) = self.chart.clone() else {
            return;
        };
        // A click inside the hub walks back up (sunburst only — the flat views
        // have no hub).
        let in_hub = if self.view == View::Rings {
            match self.placed.lock() {
                Ok(slot) => {
                    let local_x = f32::from(event.position.x - slot.origin.x);
                    let local_y = f32::from(event.position.y - slot.origin.y);
                    let dx = local_x - slot.geom.cx;
                    let dy = local_y - slot.geom.cy;
                    (dx * dx + dy * dy).sqrt() < slot.geom.inner
                }
                Err(_) => false,
            }
        } else {
            false
        };
        if in_hub {
            self.go_up(cx);
            return;
        }
        if let Some((index, _, _)) = self.pick(event.position) {
            let Some(node) = chart.slices.get(index).map(|slice| slice.node.clone()) else {
                return;
            };
            if node.is_dir() {
                self.set_focus(node, cx);
            } else {
                self.note = Some(format!("{} · {}", node.name, theme::format_bytes(node.size)));
                cx.notify();
            }
        }
    }

    fn on_chart_right_click(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A right press on an open menu just dismisses it.
        if let Some(menu) = self.menu.clone() {
            self.menu = None;
            self.menu_hover = None;
            if menu_contains(&menu, f32::from(event.position.x), f32::from(event.position.y)) {
                cx.notify();
                return;
            }
        }
        let picked = self.pick(event.position);
        if let Some((index, x, y)) = picked {
            let chart = self.chart.clone();
            if let Some(chart) = chart {
                let Some(node) = chart.slices.get(index).map(|slice| slice.node.clone()) else {
                    return;
                };
                if node.kind != Kind::Other && !node.path.is_empty() {
                    // Keep the card inside the window: it is painted by the
                    // canvas, which cannot scroll itself back into view.
                    let viewport = window.viewport_size();
                    let max_x = f32::from(viewport.width) - MENU_W - 8.0;
                    let max_y = f32::from(viewport.height) - MENU_H - 8.0;
                    self.menu = Some(Menu {
                        x: x.min(max_x).max(8.0),
                        y: y.min(max_y).max(8.0),
                        node,
                    });
                    // The tooltip would otherwise sit on top of the card.
                    self.hover = None;
                    self.tooltip = None;
                    self.menu_hover = None;
                    cx.notify();
                }
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                if self.update_open {
                    self.close_update(cx);
                } else if self.menu.is_some() {
                    self.menu = None;
                } else if matches!(self.phase, Phase::Scanning) {
                    self.stop_scan(cx);
                } else {
                    self.hover = None;
                    self.tooltip = None;
                }
                cx.notify();
            }
            "backspace" | "arrowup" | "arrowleft" => self.go_up(cx),
            // Refresh the folder in the focus card, or the volume at the root.
            "f5" => self.rescan_focus(cx),
            "r" if event.keystroke.modifiers.control => self.rescan_focus(cx),
            // 1 / 2 / 3 / 4 / 5 / 6 pick the view, like the toolbar buttons.
            "1" => self.set_view(View::Rings, cx),
            "2" => self.set_view(View::Treemap, cx),
            "3" => self.set_view(View::Heatmap, cx),
            "4" => self.set_view(View::Details, cx),
            "5" => self.set_view(View::Files, cx),
            "6" => self.set_view(View::Types, cx),
            // Enter opens what is selected: a folder in the details table, or
            // the folder holding the selected file in the file list.
            "enter" => match self.view {
                View::Details => {
                    if let Some(node) = self.selected_detail_node() {
                        self.detail_open(node, cx);
                    }
                }
                View::Files => {
                    if let Some(hit) = self.selected_file() {
                        self.open_containing(&hit, cx);
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }

    /// The focused folder's path, one clickable segment per level. Clicking a
    /// segment re-centres the chart on that folder.
    fn path_crumbs(&self, path: &str, cx: &mut Context<Self>) -> Div {
        let mut segments = breadcrumb_segments(path);
        // Deep paths would wrap the card: keep the tail and fold the rest.
        let mut folded = false;
        if segments.len() > 6 {
            segments = segments.split_off(segments.len() - 5);
            folded = true;
        }
        let last = segments.len().saturating_sub(1);

        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(1.0))
            .text_size(px(10.0))
            .text_color(theme::text_faint());
        if folded {
            row = row.child(div().px(px(2.0)).child("…"));
        }
        for (index, (label, full)) in segments.into_iter().enumerate() {
            if index > 0 || folded {
                row = row.child(div().px(px(1.0)).opacity(0.45).child("\\"));
            }
            let color = if index == last {
                theme::text_dim()
            } else {
                theme::text_faint()
            };
            row = row.child(
                div()
                    .id(SharedString::from(format!("crumb-{index}")))
                    .px(px(3.0))
                    .py(px(1.0))
                    .rounded(px(3.0))
                    .cursor_pointer()
                    .text_color(color)
                    .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to(full.clone(), cx);
                    }))
                    .child(label),
            );
        }
        row
    }

    fn panel(&self, cx: &mut Context<Self>) -> Div {
        let Some(chart) = self.chart.clone() else {
            return div().w(px(PANEL_W)).flex_shrink_0();
        };
        let focus = chart.focus.clone();
        let metric = self.metric;
        // `chart.total` is the focused folder itself, so "of volume" has to be
        // measured against the scanned root instead.
        let volume = self
            .tree
            .as_ref()
            .map(|tree| metric.weight(tree))
            .unwrap_or(chart.total);
        let of_total = if volume > 0.0 {
            metric.weight(&focus) / volume
        } else {
            0.0
        };

        let mut rows: Vec<AnyElement> = Vec::new();
        let top: Vec<_> = focus.children.iter().take(14).collect();
        for (index, child) in top.iter().enumerate() {
            let share = if chart.total > 0.0 {
                (metric.weight(child) / chart.total).clamp(0.0, 1.0) as f32
            } else {
                0.0
            };
            let value = match metric {
                Metric::Bytes => theme::format_bytes(child.size),
                Metric::Files => theme::format_count(child.files),
            };
            let name = short_name(&child.name, 26);
            let child_node = (*child).clone();
            let click_node = child_node.clone();
            let right_node = child_node.clone();
            let slice = chart
                .slices
                .iter()
                .position(|slice| Arc::ptr_eq(&slice.node, child));
            let hue = slice
                .map(|index| chart.slices[index].hue)
                .unwrap_or(206.0);
            let color = if child.kind == Kind::Other {
                theme::rest_color(1)
            } else {
                theme::slice_color(hue, 1)
            };
            // One hover state for both sides of the window: pointing at a row
            // lights that folder's arc (and the other way round), so the list
            // and the ring never describe two different folders.
            let is_hover = slice.is_some() && self.hover == slice;

            rows.push(
                div()
                    .id(SharedString::from(format!("row-{index}")))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .px(px(7.0))
                    .py(px(5.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .bg(if is_hover {
                        theme::surface_alt()
                    } else {
                        theme::surface()
                    })
                    .hover(|style| style.bg(theme::surface_alt()))
                    .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                        if let Some(index) = slice {
                            this.hover_folder(index, cx);
                        }
                    }))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        // Only this row's own highlight is cleared, so sliding
                        // from row to row never blanks the ring in between.
                        if !*hovered && slice.is_some() && this.hover == slice {
                            this.clear_hover();
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if click_node.is_dir() {
                            this.set_focus(click_node.clone(), cx);
                        }
                    }))
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                            if right_node.kind != Kind::Other && !right_node.path.is_empty() {
                                this.menu = Some(Menu {
                                    x: f32::from(event.position.x),
                                    y: f32::from(event.position.y),
                                    node: right_node.clone(),
                                });
                                cx.notify();
                            }
                        }),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.5))
                                    .text_color(if child.kind == Kind::Other {
                                        theme::text_dim()
                                    } else {
                                        theme::text()
                                    })
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_size(px(10.5))
                                    .text_color(theme::text_dim())
                                    .child(value),
                            ),
                    )
                    .child(
                        div()
                            .h(px(3.0))
                            .w_full()
                            .rounded_full()
                            .bg(theme::track())
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(color)
                                    .w(px((PANEL_W - 46.0) * share)),
                            ),
                    )
                    .into_any_element(),
            );
        }

        div()
            .w(px(PANEL_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child({
                let at_root = focus.path.eq_ignore_ascii_case(&self.last_root);
                let mut card = div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .p(px(12.0))
                    .rounded(px(12.0))
                    .bg(theme::surface())
                    .border_1()
                    .border_color(theme::hairline())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(focus.name.clone()),
                            ),
                    );
                // At the scan root the title already says where you are and
                // there is nowhere to go up to, so the path row earns nothing.
                if !at_root {
                    card = card.child(
                        div()
                            .mt(px(2.0))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(self.ghost_button("↑ Up", true, cx, |this, cx| this.go_up(cx)))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(self.path_crumbs(&focus.path, cx)),
                            ),
                    );
                }
                card.child(
                    div()
                        .mt(px(6.0))
                        .flex()
                        .gap(px(18.0))
                        .child(stat("SIZE", theme::format_bytes(focus.size)))
                        .child(stat("FILES", theme::format_count(focus.files)))
                        .child(stat("FOLDERS", theme::format_count(focus.dirs)))
                        .child(stat("OF VOLUME", theme::format_percent(of_total as f64))),
                )
                .child(
                    div()
                        .mt(px(9.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(self.action_button(self.rescan_label(), cx, |this, cx| {
                            this.rescan_focus(cx);
                        }))
                        .child(self.action_button("↗ Reveal", cx, |this, _| {
                            if let Some(focus) = this.focus() {
                                crate::sys::reveal(&focus.path, focus.is_dir());
                            }
                        }))
                        .child(self.danger_button(
                            "🗑 Delete…",
                            self.focus().is_some_and(|focus| {
                                focus.kind != Kind::Other
                                    && !focus.path.is_empty()
                                    && !focus.path.eq_ignore_ascii_case(&self.last_root)
                            }),
                            cx,
                            |this, cx| {
                                if let Some(focus) = this.focus() {
                                    this.ask_delete(focus, cx);
                                }
                            },
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px(px(2.0))
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme::text_faint())
                            .child(format!("BREAKDOWN · {} ITEMS", focus.children.len())),
                    )
                    .child(
                        div()
                            .text_size(px(10.0))
                            .text_color(theme::text_faint())
                            .child("right-click for actions"),
                    ),
            )
            .child(
                div()
                    .id("breakdown")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .overflow_hidden()
                    .children(rows),
            )
    }

    // --------------------------------------------------------------- updates

    /// Look for a newer release a moment after the window opens. A machine with
    /// no network stays quiet: only a release that is actually out lights the
    /// version chip, so a failed background check leaves no trace.
    fn check_for_updates_soon(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(2500))
                .await;
            let _ = this.update(cx, |this, cx| this.start_update_check(false, cx));
        })
        .detach();
    }

    /// Ask GitHub for the newest release. `open` decides whether the card comes
    /// up with the answer: a check the reader asked for answers in the card, a
    /// background one only lights the chip.
    fn start_update_check(&mut self, open: bool, cx: &mut Context<Self>) {
        if matches!(self.update, Update::Checking | Update::Downloading { .. }) {
            self.update_open = true;
            cx.notify();
            return;
        }
        self.update = Update::Checking;
        self.update_open = open;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let answer = cx.background_spawn(async move { update::check() }).await;
            let _ = this.update(cx, |this, cx| this.finish_update_check(answer, cx));
        })
        .detach();
    }

    fn finish_update_check(
        &mut self,
        answer: Result<update::Release, String>,
        cx: &mut Context<Self>,
    ) {
        self.update = match answer {
            Ok(release) if update::is_newer(&release.version, update::VERSION) => {
                Update::Available(Arc::new(release))
            }
            Ok(_) => Update::Current,
            // A check nobody asked for should not leave a complaint in the bar
            // for the rest of the session; the reader can ask again by hand.
            Err(_) if !self.update_open => Update::Idle,
            Err(error) => Update::Failed(error),
        };
        cx.notify();
    }

    /// Fetch the release's archive for this platform, repainting the card as the
    /// bytes arrive.
    fn start_download(&mut self, release: Arc<update::Release>, cx: &mut Context<Self>) {
        let Some(asset) = release.asset.clone() else {
            // Nothing to fetch for this platform: the page at least says why.
            crate::sys::open_url(&release.page);
            self.note = Some(format!("{} has no build for this platform", release.tag));
            cx.notify();
            return;
        };
        let Some(dir) = updates_dir() else {
            self.update = Update::Failed("this machine has no folder for updates".to_string());
            cx.notify();
            return;
        };

        let cancel = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(Mutex::new((0u64, 0u64)));
        self.update = Update::Downloading {
            release,
            progress: progress.clone(),
            cancel: cancel.clone(),
        };
        self.note = Some(format!("Downloading {}", asset.name));
        cx.notify();

        // Repaint while the bytes come in; the loop dies with the state.
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let still_downloading = this.update(cx, |this, cx| {
                if matches!(this.update, Update::Downloading { .. }) {
                    cx.notify();
                    true
                } else {
                    false
                }
            });
            if !matches!(still_downloading, Ok(true)) {
                break;
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let report_progress = progress.clone();
            let answer = cx
                .background_spawn(async move {
                    let mut report = |done: u64, total: u64| {
                        if let Ok(mut slot) = report_progress.lock() {
                            *slot = (done, total);
                        }
                    };
                    update::download(&asset, &dir, &cancel, &mut report)
                })
                .await;
            let _ = this.update(cx, |this, cx| this.finish_download(answer, cx));
        })
        .detach();
    }

    fn finish_download(
        &mut self,
        answer: Result<std::path::PathBuf, String>,
        cx: &mut Context<Self>,
    ) {
        let release = match &self.update {
            Update::Downloading { release, .. } => release.clone(),
            _ => return,
        };
        self.update = match answer {
            Ok(archive) => {
                self.note = Some("Update downloaded — restart to install it".to_string());
                Update::Ready { release, archive }
            }
            Err(error) if error == "Download cancelled" => {
                self.note = Some("Download cancelled".to_string());
                Update::Available(release)
            }
            Err(error) => Update::Failed(error),
        };
        self.update_open = true;
        cx.notify();
    }

    /// Hand over to the installer script and leave: it waits for this process,
    /// swaps the binary and starts the new build.
    fn install_update(&mut self, archive: std::path::PathBuf, cx: &mut Context<Self>) {
        match update::apply(&archive) {
            Ok(()) => cx.quit(),
            Err(error) => {
                self.update = Update::Failed(error);
                cx.notify();
            }
        }
    }

    /// Close the card. A download in flight is cancelled with it — the card is
    /// the only place that shows its progress.
    fn close_update(&mut self, cx: &mut Context<Self>) {
        if let Update::Downloading { release, cancel, .. } = &self.update {
            cancel.store(true, Ordering::Relaxed);
            self.update = Update::Available(release.clone());
        }
        self.update_open = false;
        cx.notify();
    }

    /// What the status bar's version chip says, one line for every state.
    fn update_chip_label(&self) -> String {
        match &self.update {
            Update::Idle | Update::Current | Update::Failed(_) => {
                format!("v{}", update::VERSION)
            }
            Update::Checking => "checking for updates…".to_string(),
            Update::Available(release) => format!("↓ {} available", release.tag),
            Update::Downloading { release, progress, .. } => {
                let (done, total) = progress.lock().map(|slot| *slot).unwrap_or((0, 0));
                let share = if total > 0 {
                    done as f64 / total as f64
                } else {
                    0.0
                };
                format!("↓ {} · {}", release.tag, theme::format_percent(share))
            }
            Update::Ready { release, .. } => format!("↻ restart to update to {}", release.tag),
        }
    }

    /// The version in the status bar, which doubles as the way into the update
    /// card: click it to check now, or to open what the check found.
    fn version_chip(&self, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let loud = matches!(
            self.update,
            Update::Available(_) | Update::Downloading { .. } | Update::Ready { .. }
        );
        div()
            .id("update-chip")
            .px(px(7.0))
            .py(px(2.0))
            .rounded(px(6.0))
            .text_size(px(10.5))
            .cursor_pointer()
            .bg(if loud {
                theme::surface_alt()
            } else {
                theme::surface()
            })
            .border_1()
            .border_color(theme::hairline())
            .text_color(if loud {
                theme::accent()
            } else {
                theme::text_faint()
            })
            .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
            .on_click(cx.listener(|this, _, _, cx| this.on_update_chip(cx)))
            .child(self.update_chip_label())
    }

    fn on_update_chip(&mut self, cx: &mut Context<Self>) {
        match &self.update {
            Update::Available(_) | Update::Ready { .. } => {
                self.update_open = true;
                cx.notify();
            }
            Update::Checking | Update::Downloading { .. } => {
                self.update_open = true;
                cx.notify();
            }
            // Nothing to show yet, or the last answer is stale: ask again and
            // let the card hold the answer.
            Update::Idle | Update::Current | Update::Failed(_) => {
                self.start_update_check(true, cx);
            }
        }
    }

    /// A button for the update card. The primary one carries the accent; the
    /// rest sit on the card's own surface.
    fn update_button(
        &self,
        label: &str,
        primary: bool,
        cx: &mut Context<Self>,
        action: impl Fn(&mut DiskApp, &mut Context<DiskApp>) + 'static,
    ) -> gpui::Stateful<Div> {
        div()
            .id(SharedString::from(format!("update-{label}")))
            .px(px(12.0))
            .py(px(6.0))
            .rounded(px(7.0))
            .text_size(px(11.5))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .bg(if primary {
                theme::accent_soft()
            } else {
                theme::surface_alt()
            })
            .border_1()
            .border_color(if primary {
                theme::accent()
            } else {
                theme::hairline()
            })
            .text_color(if primary {
                theme::accent()
            } else {
                theme::text_dim()
            })
            .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
            .on_click(cx.listener(move |this, _, _, cx| action(this, cx)))
            .child(label.to_string())
    }

    /// The update card, over a scrim that dims the window behind it.
    fn update_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let running = update::VERSION;
        let mut card = div()
            .id("update-card")
            .w(px(470.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(18.0))
            .rounded(px(12.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            // A press inside the card must not reach the scrim behind it.
            .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()));

        let (heading, line) = match &self.update {
            Update::Idle => (
                "Check for updates".to_string(),
                format!("Disk Scanner {running}"),
            ),
            Update::Checking => (
                "Checking for updates".to_string(),
                format!("Asking GitHub for the newest release… Disk Scanner {running}"),
            ),
            Update::Current => (
                "You're up to date".to_string(),
                format!("Disk Scanner {running} is the newest release."),
            ),
            Update::Available(release) => (
                "Update available".to_string(),
                format!("{} is out — you are running {running}.", release.tag),
            ),
            Update::Downloading { release, .. } => (
                "Downloading update".to_string(),
                format!("{} is on its way.", release.tag),
            ),
            Update::Ready { release, .. } => (
                "Update ready".to_string(),
                format!(
                    "{} is downloaded. This window will close, the new build will take the old one's place and start again.",
                    release.tag
                ),
            ),
            Update::Failed(error) => ("Update check failed".to_string(), error.clone()),
        };

        card = card
            .child(
                div()
                    .text_size(px(15.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(heading),
            )
            .child(
                div()
                    .text_size(px(11.5))
                    .text_color(theme::text_dim())
                    .child(line),
            );

        match &self.update {
            Update::Available(release) => {
                let detail = match &release.asset {
                    Some(asset) => format!(
                        "{} · {}",
                        asset.name,
                        theme::format_bytes(asset.size)
                    ),
                    None => "This release has no build for this platform — the release page has the source.".to_string(),
                };
                card = card.child(
                    div()
                        .text_size(px(10.5))
                        .text_color(theme::text_faint())
                        .child(detail),
                );
            }
            Update::Downloading { progress, .. } => {
                let (done, total) = progress.lock().map(|slot| *slot).unwrap_or((0, 0));
                let share = if total > 0 {
                    (done as f32 / total as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                card = card
                    .child(
                        div()
                            .h(px(4.0))
                            .w_full()
                            .rounded_full()
                            .bg(theme::track())
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(theme::accent())
                                    .w(relative(share)),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme::text_faint())
                            .child(format!(
                                "{} of {}",
                                theme::format_bytes(done),
                                theme::format_bytes(total)
                            )),
                    );
            }
            Update::Ready { archive, .. } => {
                card = card.child(
                    div()
                        .truncate()
                        .text_size(px(10.5))
                        .text_color(theme::text_faint())
                        .child(archive.to_string_lossy().into_owned()),
                );
            }
            _ => {}
        }

        let mut actions = div()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(8.0))
            .child(self.update_button("Close", false, cx, |this, cx| this.close_update(cx)));

        match &self.update {
            Update::Available(release) => {
                if release.asset.is_none() {
                    let page = release.page.clone();
                    actions = actions.child(self.update_button(
                        "Open the release page",
                        true,
                        cx,
                        move |_, _| crate::sys::open_url(&page),
                    ));
                } else {
                    let release = release.clone();
                    actions = actions.child(self.update_button(
                        "Download and install",
                        true,
                        cx,
                        move |this, cx| this.start_download(release.clone(), cx),
                    ));
                }
            }
            Update::Downloading { .. } => {
                actions = actions.child(self.update_button("Cancel download", false, cx, |this, cx| {
                    this.close_update(cx)
                }));
            }
            Update::Ready { archive, .. } => {
                let archive = archive.clone();
                actions = actions.child(self.update_button(
                    "Restart and install",
                    true,
                    cx,
                    move |this, cx| this.install_update(archive.clone(), cx),
                ));
            }
            Update::Idle | Update::Current | Update::Failed(_) => {
                actions = actions.child(self.update_button("Check again", true, cx, |this, cx| {
                    this.start_update_check(true, cx)
                }));
            }
            Update::Checking => {}
        }

        card = card.child(actions);

        let (width, height) = self.viewport;
        div()
            .id("update-scrim")
            .absolute()
            .left(px(0.0))
            .top(px(0.0))
            .w(px(width))
            .h(px(height))
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim())
            .on_click(cx.listener(|this, _, _, cx| this.close_update(cx)))
            .child(card)
            .into_any_element()
    }

    fn status_bar(&self, cx: &mut Context<Self>) -> Div {
        // The card over the chart already carries the live counters, the path
        // being walked and the Stop button, so the bar states the phase and the
        // way out of it instead of repeating the same numbers underneath.
        let (left, right) = match &self.phase {
            Phase::Empty => ("Ready".to_string(), String::new()),
            Phase::Scanning => (
                "Scanning…".to_string(),
                "Esc or Stop to pause".to_string(),
            ),
            Phase::Stopped => (
                "Scan stopped".to_string(),
                self.note.clone().unwrap_or_default(),
            ),
            Phase::Ready {
                elapsed,
                files,
                bytes,
            } => (
                format!(
                    "Scanned in {} · {} files · {}",
                    theme::format_seconds(*elapsed),
                    theme::format_count(*files),
                    theme::format_bytes(*bytes)
                ),
                self.note.clone().unwrap_or_default(),
            ),
            Phase::Failed(error) => (error.clone(), String::new()),
        };

        div()
            .h(px(STATUSBAR_H))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_between()
            .px(px(PAD + 4.0))
            .text_size(px(10.5))
            .text_color(theme::text_faint())
            .child(div().truncate().child(left))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .child(div().truncate().child(right))
                    .child(self.version_chip(cx)),
            )
    }

    fn overlays(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut layers: Vec<AnyElement> = Vec::new();

        if let (Some(tooltip), Some(chart)) = (&self.tooltip, &self.chart) {
            if let Some(slice) = chart.slices.get(tooltip.index) {
                let node = &slice.node;
                let share = if chart.total > 0.0 {
                    chart.metric.weight(node) / chart.total
                } else {
                    0.0
                };
                // Keep the card on screen: the pointer can sit at the very edge
                // of the window, and a tooltip half outside it is unreadable.
                let max_x = (self.viewport.0 - TOOLTIP_W - 8.0).max(8.0);
                let max_y = (self.viewport.1 - TOOLTIP_H - 8.0).max(8.0);
                let x = (tooltip.x + 14.0).min(max_x).max(8.0);
                let y = (tooltip.y + 16.0).min(max_y).max(8.0);
                layers.push(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(TOOLTIP_W))
                        .p(px(9.0))
                        .rounded(px(9.0))
                        .bg(theme::surface_alt())
                        .border_1()
                        .border_color(theme::hairline())
                        .flex()
                        .flex_col()
                        .gap(px(3.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .font_weight(FontWeight::MEDIUM)
                                .child(node.name.clone()),
                        )
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme::text_faint())
                                .truncate()
                                .child(if node.path.is_empty() {
                                    "aggregated slice".to_string()
                                } else {
                                    node.path.clone()
                                }),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme::text_dim())
                                .child(format!(
                                    "{} · {} of {} · {}",
                                    theme::format_bytes(node.size),
                                    theme::format_percent(share),
                                    match chart.metric {
                                        Metric::Bytes => "folder",
                                        Metric::Files => "parent",
                                    },
                                    if node.files == 1 {
                                        "1 file".to_string()
                                    } else {
                                        format!("{} files", theme::format_count(node.files))
                                    }
                                )),
                        )
                        .into_any_element(),
                );
            }
        }


        if self.update_open {
            layers.push(self.update_card(cx));
        }

        layers
    }
}

/// Splits an absolute path into `(label, full path)` pairs, one per level, so
/// the panel can render it as a breadcrumb.
#[cfg(windows)]
fn breadcrumb_segments(path: &str) -> Vec<(String, String)> {
    let trimmed = path.trim_end_matches('\\');
    // UNC shares and verbatim paths keep a single, unsplittable label.
    if trimmed.is_empty() || trimmed.starts_with("\\\\") || !trimmed.contains('\\') {
        let full = if trimmed.contains('\\') || trimmed.is_empty() {
            path.to_string()
        } else {
            format!("{trimmed}\\")
        };
        return vec![(trimmed.to_string(), full)];
    }
    let mut out: Vec<(String, String)> = Vec::new();
    let mut prefix = String::new();
    for (index, part) in trimmed.split('\\').enumerate() {
        if part.is_empty() {
            continue;
        }
        if index == 0 {
            prefix = format!("{part}\\");
        } else if prefix.ends_with('\\') {
            prefix.push_str(part);
        } else {
            prefix.push('\\');
            prefix.push_str(part);
        }
        out.push((part.to_string(), prefix.clone()));
    }
    if out.is_empty() {
        return vec![(trimmed.to_string(), path.to_string())];
    }
    out
}

/// Same contract for POSIX paths: `/` is the first level, then one entry per
/// directory below it.
#[cfg(not(windows))]
fn breadcrumb_segments(path: &str) -> Vec<(String, String)> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return vec![("/".to_string(), "/".to_string())];
    }
    let mut out: Vec<(String, String)> = vec![("/".to_string(), "/".to_string())];
    let mut prefix = String::new();
    for part in trimmed.split('/').filter(|part| !part.is_empty()) {
        prefix.push('/');
        prefix.push_str(part);
        out.push((part.to_string(), prefix.clone()));
    }
    out
}

fn stat(label: &str, value: String) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .text_size(px(8.5))
                .text_color(theme::text_faint())
                .child(label.to_string()),
        )
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .child(value),
        )
}

/// A small all-caps heading for the details view's navigation pane.
fn section_label(text: &str) -> Div {
    div()
        .px(px(7.0))
        .py(px(3.0))
        .text_size(px(9.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme::text_faint())
        .child(text.to_string())
}

/// `1 file` / `4 files`, for the details footer.
fn count_label(count: usize, one: &str, many: &str) -> String {
    if count == 1 {
        format!("1 {one}")
    } else {
        format!("{count} {many}")
    }
}

/// One fixed-width column of a details row.
fn detail_cell(text: String, width: f32, right: bool) -> Div {
    let mut cell = div()
        .w(px(width))
        .flex_shrink()
        .px(px(6.0))
        .truncate()
        .text_size(px(11.0))
        .text_color(theme::text_dim());
    if right {
        cell = cell.text_right();
    }
    cell.child(text)
}

/// The "Type" column: what Explorer would call this row.
fn type_label(node: &Node) -> String {
    if node.kind == Kind::Dir {
        return "File folder".to_string();
    }
    if node.kind == Kind::Other {
        return "—".to_string();
    }
    let extension = extension_of(&node.name);
    if extension.is_empty() {
        return "File".to_string();
    }
    match extension.as_str() {
        "txt" | "log" | "rtf" => "Text Document".to_string(),
        "md" => "Markdown Document".to_string(),
        "pdf" => "PDF Document".to_string(),
        "doc" | "docx" => "Word Document".to_string(),
        "xls" | "xlsx" | "csv" => "Excel Worksheet".to_string(),
        "ppt" | "pptx" => "PowerPoint Presentation".to_string(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "svg" | "ico" => "Image".to_string(),
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "wmv" => "Video".to_string(),
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => "Audio".to_string(),
        "zip" | "7z" | "rar" | "gz" | "tar" | "cab" => "Compressed Archive".to_string(),
        "exe" => "Application".to_string(),
        "dll" => "Application Extension".to_string(),
        "sys" | "drv" => "System File".to_string(),
        "iso" | "img" => "Disc Image".to_string(),
        "json" => "JSON File".to_string(),
        "xml" => "XML Document".to_string(),
        "html" | "htm" => "HTML Document".to_string(),
        "rs" => "Rust Source File".to_string(),
        "cs" => "C# Source File".to_string(),
        "js" => "JavaScript File".to_string(),
        "ts" => "TypeScript File".to_string(),
        "py" => "Python Source File".to_string(),
        "ps1" => "PowerShell Script".to_string(),
        "bat" | "cmd" => "Batch File".to_string(),
        other => format!("{} File", other.to_ascii_uppercase()),
    }
}

/// Lower-case extension without the dot, or an empty string for a name that
/// has none (or only a leading dot).
fn extension_of(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            extension.to_ascii_lowercase()
        }
        _ => String::new(),
    }
}

/// The file name inside a scanned path.
fn file_name(path: &str) -> &str {
    match path.rfind(['\\', '/']) {
        Some(index) if index + 1 < path.len() => &path[index + 1..],
        Some(_) => path,
        None => path,
    }
}

/// The folder that holds a scanned path, without a trailing separator (except
/// for a drive root, which is "C:\" and not "C:").
fn file_folder(path: &str) -> &str {
    let Some(index) = path.rfind(['\\', '/']) else {
        return "";
    };
    if index == 0 {
        return &path[..1];
    }
    let head = &path[..index];
    if head.ends_with(':') {
        return &path[..index + 1];
    }
    head
}

/// The tail of a folder path, so the part that tells two long paths apart
/// stays visible: `…\disk-scanner\crates\disk-scan`.
fn short_folder(path: &str, max: usize) -> String {
    let characters = path.chars().count();
    if characters <= max {
        return path.to_string();
    }
    let skip = characters - max + 1;
    format!("…{}", path.chars().skip(skip).collect::<String>())
}

/// A file from the scan-wide list, as a tree node, so deleting it can reuse the
/// same confirmation, recycle-bin and tree-pruning path as a chart click.
fn file_node(hit: &FileHit) -> Arc<Node> {
    Arc::new(Node {
        name: file_name(&hit.path).to_string(),
        path: hit.path.clone(),
        size: hit.size,
        files: 1,
        dirs: 0,
        kind: Kind::File,
        children: Vec::new(),
    })
}

/// A file's last-write time as the details table would print it.
fn format_index_date(seconds: i64) -> String {
    if seconds < 0 {
        return "—".to_string();
    }
    let stamp = UNIX_EPOCH + Duration::from_secs(seconds as u64);
    crate::sys::format_timestamp(stamp).unwrap_or_else(|| "—".to_string())
}

/// Take the copies under a deleted path out of the duplicate sets, and drop the
/// sets that no longer hold at least two copies.
fn prune_duplicate_copies(scan: &DuplicateScan, target: &str) -> DuplicateScan {
    let gone = |path: &str| is_inside(target, path) || path.eq_ignore_ascii_case(target);
    let mut pruned = DuplicateScan {
        groups: scan
            .groups
            .iter()
            .filter_map(|group| {
                let copies: Vec<FileHit> = group
                    .copies
                    .iter()
                    .filter(|hit| !gone(&hit.path))
                    .cloned()
                    .collect();
                if copies.len() >= 2 {
                    Some(DupGroup {
                        size: group.size,
                        copies,
                    })
                } else {
                    None
                }
            })
            .collect(),
        hashed: scan.hashed,
        cancelled: scan.cancelled,
    };
    pruned.groups.sort_by(|left, right| {
        let wasted = |group: &DupGroup| {
            group.size * group.copies.len().saturating_sub(1) as u64
        };
        wasted(right)
            .cmp(&wasted(left))
            .then_with(|| right.size.cmp(&left.size))
    });
    pruned
}

/// One stable colour per file category, reused by the kind dot, the type bars
/// and the list rows.
fn category_color(category: Category) -> Hsla {
    theme::slice_color(theme::root_hue((category.index() * 3) % 14), 1)
}

/// Where an exported CSV goes: the usual Downloads folder when there is one.
fn export_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from);
    if let Some(home) = home {
        let downloads = home.join("Downloads");
        if downloads.is_dir() {
            return downloads;
        }
    }
    config_dir().unwrap_or_else(std::env::temp_dir)
}

/// Writes the file list as CSV. Quoted only where it has to be: a path with a
/// comma or a quote is rare, and the markdown-free output stays readable.
fn write_file_csv(path: &Path, rows: &[FileHit]) -> std::io::Result<()> {
    use std::io::Write;
    let mut text = String::with_capacity(rows.len() * 96 + 64);
    text.push_str("Name,Folder,Size,Modified,Kind\n");
    for hit in rows {
        text.push_str(&csv_cell(file_name(&hit.path)));
        text.push(',');
        text.push_str(&csv_cell(file_folder(&hit.path)));
        text.push(',');
        text.push_str(&hit.size.to_string());
        text.push(',');
        text.push_str(&csv_cell(&format_index_date(hit.modified)));
        text.push(',');
        text.push_str(Category::of_name(file_name(&hit.path)).label());
        text.push('\n');
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()
}

fn csv_cell(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

/// Draws the context menu onto the chart canvas.
///
/// The card is painted from the canvas rather than a popup layer because popup
/// layers in this GPUI build paint correctly but never receive presses; presses
/// are resolved from the pointer position instead.
fn paint_menu(
    window: &mut Window,
    _origin: Point<Pixels>,
    menu: &Menu,
    hover: Option<usize>,
) {
    // Canvas paint coordinates are window coordinates, like `paint_sunburst`.
    let x = px(menu.x);
    let y = px(menu.y);

    let card = Bounds {
        origin: point(x, y),
        size: size(px(MENU_W), px(MENU_H)),
    };
    window.paint_quad(quad(
        card,
        px(9.0),
        theme::surface_alt(),
        px(1.0),
        theme::hairline(),
        BorderStyle::Solid,
    ));

    for (index, top) in [(0usize, MENU_REVEAL_TOP), (1usize, MENU_DELETE_TOP)] {
        if hover == Some(index) {
            let row = Bounds {
                origin: point(x + px(4.0), y + px(top)),
                size: size(px(MENU_W - 8.0), px(MENU_ROW_H)),
            };
            window.paint_quad(quad(
                row,
                px(6.0),
                theme::hairline(),
                px(0.0),
                theme::hairline(),
                BorderStyle::Solid,
            ));
        }
    }

    // Divider between "Reveal" and the destructive row.
    let divider = Bounds {
        origin: point(x + px(6.0), y + px(MENU_REVEAL_TOP + MENU_ROW_H + 3.0)),
        size: size(px(MENU_W - 12.0), px(1.0)),
    };
    window.paint_quad(quad(
        divider,
        px(0.0),
        theme::hairline(),
        px(0.0),
        theme::hairline(),
        BorderStyle::Solid,
    ));
}

/// Text for the painted menu: one absolutely placed label per line.
fn menu_label(
    menu: &Menu,
    origin: Point<Pixels>,
    top: f32,
    text: String,
    color: gpui::Rgba,
    weight: FontWeight,
) -> AnyElement {
    let x = menu.x - f32::from(origin.x);
    let y = menu.y - f32::from(origin.y);
    div()
        .absolute()
        .left(px(x + 9.0))
        .top(px(y + top))
        .text_size(px(11.5))
        .text_color(color)
        .font_weight(weight)
        .child(text)
        .into_any_element()
}

fn ancestors(chart: &ChartModel, index: usize) -> HashSet<usize> {
    let mut set = HashSet::new();
    // A hover index from the previous chart (before a volume or folder switch)
    // can point past the new slice list; drop it instead of panicking.
    if index >= chart.slices.len() {
        return set;
    }
    let mut current = index;
    loop {
        set.insert(current);
        let parent = chart.slices[current].parent;
        if parent == usize::MAX || parent >= chart.slices.len() {
            break;
        }
        current = parent;
    }
    set
}

/// Paint the treemap / heatmap cells. Treemap cells get a diagonal cushion
/// gradient so nested rectangles read as separate blocks; heatmap cells are
/// flat fills whose hue comes from the log of the item's weight.
fn paint_tiles(
    window: &mut Window,
    origin: Point<Pixels>,
    tiles: &[Tile],
    chart: &ChartModel,
    view: View,
    hover: Option<usize>,
    chain: &HashSet<usize>,
) {
    let dimming = hover.is_some();

    // Weight range for the heat ramp.
    let mut min = f64::MAX;
    let mut max = 0.0_f64;
    let mut weights = Vec::with_capacity(chart.slices.len());
    for slice in &chart.slices {
        let weight = chart.metric.weight(&slice.node);
        weights.push(weight);
        min = min.min(weight);
        max = max.max(weight);
    }
    if min == f64::MAX {
        min = 0.0;
    }

    for (index, tile) in tiles.iter().enumerate() {
        if tile.w < 0.6 || tile.h < 0.6 {
            continue;
        }
        let slice = &chart.slices[index];
        let mut color = match view {
            View::Heatmap => theme::heat_color(log_rank(weights[index], min, max)),
            _ => {
                if slice.node.kind == Kind::Other {
                    theme::rest_color(slice.depth)
                } else {
                    theme::slice_color(slice.hue, slice.depth)
                }
            }
        };
        // In the heatmap the colour *is* the measurement, so dimming would
        // destroy the encoding; the hovered cell is outlined instead.
        if dimming && view != View::Heatmap && !chain.contains(&index) {
            color.a = 0.5;
        } else if dimming {
            color.a = 1.0;
        }

        let bounds = Bounds {
            origin: point(origin.x + px(tile.x), origin.y + px(tile.y)),
            size: size(px(tile.w), px(tile.h)),
        };
        let background = match view {
            View::Treemap => {
                let light = Hsla {
                    l: (color.l + 0.07).min(1.0),
                    ..color
                };
                let dark = Hsla {
                    l: (color.l - 0.09).max(0.0),
                    ..color
                };
                linear_gradient(135.0, linear_color_stop(light, 0.0), linear_color_stop(dark, 1.0))
            }
            _ => gpui::Background::from(color),
        };
        let border = Hsla {
            l: (color.l * 0.4).max(0.02),
            a: 0.85,
            ..color
        };
        let (width, stroke) = if view == View::Heatmap && hover == Some(index) {
            (1.5, Hsla { l: 0.96, s: 0.0, h: 0.0, a: 0.95 })
        } else {
            (0.5, border)
        };
        window.paint_quad(quad(
            bounds,
            px(0.0),
            background,
            px(width),
            stroke,
            BorderStyle::Solid,
        ));
    }
}

fn paint_sunburst(
    window: &mut Window,
    origin: Point<Pixels>,
    geom: &Geom,
    chart: &ChartModel,
    hover: Option<usize>,
    chain: &HashSet<usize>,
) {
    let center = point(origin.x + px(geom.cx), origin.y + px(geom.cy));
    let dimming = hover.is_some();

    for (index, slice) in chart.slices.iter().enumerate() {
        let (r0, r1) = geom.ring_bounds(slice.depth);
        let span = slice.span();
        let pad = (span * 0.06).min(0.005);
        let a0 = slice.x0 + pad;
        let a1 = slice.x1 - pad;
        if a1 <= a0 {
            continue;
        }
        // Skip slivers that would only cost tessellation time.
        if span * r1 < 1.6 {
            continue;
        }

        let mut color = if slice.node.kind == Kind::Other {
            theme::rest_color(slice.depth)
        } else {
            theme::slice_color(slice.hue, slice.depth)
        };
        if dimming && !chain.contains(&index) {
            color.a = 0.55;
        } else if dimming {
            color.a = 1.0;
        }

        if let Ok(path) = arc_path(center, r0, r1, a0, a1) {
            window.paint_path(path, color);
        }
    }
}

fn arc_path(
    center: Point<Pixels>,
    r0: f32,
    r1: f32,
    a0: f32,
    a1: f32,
) -> Result<gpui::Path<Pixels>, ()> {
    let steps = (((a1 - a0) * r1 / 3.0).ceil() as usize).clamp(1, 64);
    let at = |radius: f32, angle: f32| {
        point(
            center.x + px(radius * angle.cos()),
            center.y + px(radius * angle.sin()),
        )
    };

    let mut builder = PathBuilder::fill();
    builder.move_to(at(r1, a0));
    for step in 1..=steps {
        let angle = a0 + (a1 - a0) * step as f32 / steps as f32;
        builder.line_to(at(r1, angle));
    }
    builder.line_to(at(r0, a1));
    for step in (0..steps).rev() {
        let angle = a0 + (a1 - a0) * step as f32 / steps as f32;
        builder.line_to(at(r0, angle));
    }
    builder.close();
    builder.build().map_err(|_| ())
}

#[derive(Clone, Copy, Default)]
struct Delta {
    size: i64,
    files: i64,
    dirs: i64,
}

impl Delta {
    fn between(old: &Node, new: &Node) -> Self {
        Self {
            size: new.size as i64 - old.size as i64,
            files: new.files as i64 - old.files as i64,
            dirs: new.dirs as i64 - old.dirs as i64,
        }
    }
}

/// The node one ring above `path` — the tree's own parent link, which is what
/// "Up" means on the chart.
///
/// `C:` or `G:  Google Drive` — the volume as the select and the list show it.
fn drive_title(drive: &DriveInfo) -> String {
    let root = drive.root.trim_end_matches('\\');
    if drive.label.is_empty() {
        root.to_string()
    } else {
        format!("{}  {}", root, drive.label)
    }
}

/// How much of the volume is in use, as 0..=1.
fn usage_fraction(drive: &DriveInfo) -> f32 {
    if drive.total == 0 {
        return 0.0;
    }
    let used = drive.total.saturating_sub(drive.free);
    used as f32 / drive.total as f32
}

/// A folder by absolute path.
///
/// `Node::find` also matches files, and a breadcrumb segment can name a path
/// that exists both as a small file and as the head of a folder chain
/// (`app.asar.unpacked` beside `app.asar.unpacked/dsh/node_modules`). Focusing
/// the file would centre the chart on something with no children, so only
/// folders come back from here.
fn find_dir(tree: &Arc<Node>, path: &str) -> Option<Arc<Node>> {
    let found = Node::find(tree, path)?;
    found.is_dir().then_some(found)
}

/// `to_dto` folds a chain of single-child folders into one node (`a/b/c`), so
/// path arithmetic would land on a prefix that the tree happens to expose as
/// some unrelated node (the file `app.asar.unpacked` next to the folded
/// `app.asar.unpacked/dsh/node_modules`). Walking the children instead always
/// yields the ring the user is looking at.
fn parent_of(tree: &Arc<Node>, path: &str) -> Option<Arc<Node>> {
    for child in &tree.children {
        if child.path.eq_ignore_ascii_case(path) {
            return Some(tree.clone());
        }
        if is_inside(&child.path, path) {
            if let Some(found) = parent_of(child, path) {
                return Some(found);
            }
        }
    }
    None
}

/// The nearest folder that stands for `path`, used when a breadcrumb segment
/// names the middle of a folder chain the scanner folded into one node.
///
/// The nearest one is the right answer: it is the chain itself, and opening it
/// shows the chain's children. The deepest match would jump past the segment
/// into some folder several levels further down, which is what a click on the
/// segment used to do. Files are ignored even when one shares the segment's
/// exact path — `app.asar.unpacked` exists both as a file and as the head of a
/// chain.
fn nearest_below(tree: &Arc<Node>, path: &str) -> Option<Arc<Node>> {
    let mut node = tree.clone();
    loop {
        let mut deeper: Option<Arc<Node>> = None;
        let mut exact: Option<Arc<Node>> = None;
        let mut below: Option<Arc<Node>> = None;
        for child in node.children.iter().filter(|child| child.is_dir()) {
            if is_inside(&child.path, path) {
                // This folder contains the segment: keep walking to it.
                deeper = Some(child.clone());
                break;
            }
            if exact.is_none() && child.path.eq_ignore_ascii_case(path) {
                exact = Some(child.clone());
            }
            if below.is_none() && is_inside(path, &child.path) {
                // The first folder that sits below the segment stands for it.
                below = Some(child.clone());
            }
        }
        match deeper {
            Some(child) => node = child,
            None => return exact.or(below),
        }
    }
}

/// Push the folders that belong under a volume, or nothing when it is closed.
///
/// The scan in memory normally starts at the volume root, so its children are
/// the top level of the tree. When the user scanned one folder instead, that
/// folder is the only row the tree can honestly show beneath the volume.
fn push_drive_rows(
    root: &str,
    tree: Option<&Arc<Node>>,
    expanded: &HashSet<String>,
    rows: &mut Vec<NavRow>,
) {
    if !expanded.contains(&cache_key(root)) {
        return;
    }
    let Some(tree) = tree else { return };
    if tree.path.eq_ignore_ascii_case(root) {
        for child in tree.children.iter().filter(|child| child.is_dir()) {
            if rows.len() >= MAX_NAV_ROWS {
                return;
            }
            push_nav_rows(child, 1, expanded, rows);
        }
    } else if is_inside(root, &tree.path) {
        push_nav_rows(tree, 1, expanded, rows);
    }
}

/// Push `node` and, when it is open, its folders, in display order.
fn push_nav_rows(
    node: &Arc<Node>,
    depth: usize,
    expanded: &HashSet<String>,
    rows: &mut Vec<NavRow>,
) {
    if rows.len() >= MAX_NAV_ROWS {
        return;
    }
    let has_children = node.children.iter().any(|child| child.is_dir());
    let open = has_children && expanded.contains(&cache_key(&node.path));
    rows.push(NavRow {
        node: node.clone(),
        depth,
        expanded: open,
        has_children,
    });
    if !open {
        return;
    }
    for child in node.children.iter().filter(|child| child.is_dir()) {
        if rows.len() >= MAX_NAV_ROWS {
            return;
        }
        push_nav_rows(child, depth + 1, expanded, rows);
    }
}

/// Open the root and every folder above `path`.
fn reveal_path(node: &Arc<Node>, path: &str, expanded: &mut HashSet<String>) {
    let contains = is_inside(&node.path, path);
    // The scanned root is always open: a tree that starts closed shows one row
    // and looks the way the old breadcrumb did.
    if (contains || path.eq_ignore_ascii_case(&node.path))
        && node.children.iter().any(|child| child.is_dir())
    {
        expanded.insert(cache_key(&node.path));
    }
    if !contains {
        return;
    }
    for child in node.children.iter().filter(|child| child.is_dir()) {
        reveal_path(child, path, expanded);
    }
}

/// Whether `child` lives inside `parent` (`C:\` contains `C:\Users`). The
/// comparison is case-insensitive on
/// Windows and case-sensitive elsewhere, and the separator has to follow the
/// prefix so `/home/bob` is not mistaken for a child of `/home/bo`.
fn is_inside(parent: &str, child: &str) -> bool {
    if parent.len() >= child.len() {
        return false;
    }
    let separator = std::path::MAIN_SEPARATOR;
    let (parent_bytes, child_bytes) = (parent.as_bytes(), child.as_bytes());
    let matches = child_bytes[..parent_bytes.len()]
        .iter()
        .zip(parent_bytes)
        .all(|(a, b)| if cfg!(windows) { a.eq_ignore_ascii_case(b) } else { a == b });
    if !matches {
        return false;
    }
    parent.ends_with(separator) || child_bytes[parent_bytes.len()] as char == separator
}

/// Swap the subtree at `target` for a freshly walked one, moving the size / file
/// / folder difference up through every ancestor.
fn splice(
    node: &Arc<Node>,
    target: &str,
    replacement: &Arc<Node>,
    delta: Delta,
) -> Option<Arc<Node>> {
    if node.path.eq_ignore_ascii_case(target) {
        return Some(replacement.clone());
    }

    let mut children: Vec<Arc<Node>> = Vec::with_capacity(node.children.len());
    let mut hit = false;
    for child in &node.children {
        if child.path.eq_ignore_ascii_case(target) {
            children.push(replacement.clone());
            hit = true;
            continue;
        }
        if is_inside(&child.path, target) {
            if let Some(updated) = splice(child, target, replacement, delta) {
                children.push(updated);
                hit = true;
                continue;
            }
        }
        children.push(child.clone());
    }
    if !hit {
        return None;
    }

    let files = (node.files as i64 + delta.files).max(0) as u64;
    let dirs = (node.dirs as i64 + delta.dirs).max(0) as u64;
    Some(Arc::new(Node {
        name: node.name.clone(),
        path: node.path.clone(),
        size: (node.size as i64 + delta.size).max(0) as u64,
        files,
        dirs,
        kind: node.kind,
        children,
    }))
}

fn prune(node: &Arc<Node>, target: &str) -> Arc<Node> {
    let mut removed_size = 0u64;
    let mut removed_files = 0u64;
    let mut removed_dirs = 0u64;
    let mut children: Vec<Arc<Node>> = Vec::with_capacity(node.children.len());

    for child in &node.children {
        if child.path.eq_ignore_ascii_case(target) {
            removed_size += child.size;
            removed_files += child.files;
            removed_dirs += child.dirs + 1;
            continue;
        }
        let pruned = prune(child, target);
        if pruned.size != child.size {
            removed_size += child.size - pruned.size;
            removed_files += child.files.saturating_sub(pruned.files);
            removed_dirs += child.dirs.saturating_sub(pruned.dirs);
        }
        children.push(pruned);
    }

    if removed_size == 0 && removed_files == 0 && removed_dirs == 0 {
        return node.clone();
    }

    Arc::new(Node {
        name: node.name.clone(),
        path: node.path.clone(),
        size: node.size.saturating_sub(removed_size),
        files: node.files.saturating_sub(removed_files),
        dirs: node.dirs.saturating_sub(removed_dirs),
        kind: node.kind,
        children,
    })
}

/// Where the app keeps its small state files: `%APPDATA%\disk-scanner` on
/// Windows, `$XDG_CONFIG_HOME/disk-scanner` (or `~/.config/disk-scanner`)
/// everywhere else.
#[cfg(windows)]
fn config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("APPDATA").map(|base| std::path::PathBuf::from(base).join("disk-scanner"))
}

#[cfg(not(windows))]
fn config_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".config"))
        })
        .map(|base| base.join("disk-scanner"))
}

fn last_root_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("last-root.txt"))
}

/// Where finished scans are kept on disk, so a start-up can draw the last tree
/// before the walker has read a directory.
fn snapshot_dir() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("snapshots"))
}

/// Where a downloaded update waits for the restart that installs it.
fn updates_dir() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("updates"))
}

/// Age of a cache entry in seconds, for the "from … ago" note.
fn age_seconds(at: SystemTime) -> f64 {
    SystemTime::now()
        .duration_since(at)
        .unwrap_or_default()
        .as_secs_f64()
}

fn load_last_root() -> Option<String> {
    let path = last_root_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim().to_string();
    if text.is_empty() || !Path::new(&text).exists() {
        return None;
    }
    Some(text)
}

fn save_last_root(root: &str) {
    let Some(path) = last_root_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, root);
}

/// Cache key for a scanned root: Windows paths are case-insensitive, so they
/// are folded; a POSIX path is kept as written, minus a trailing slash.
#[cfg(windows)]
fn cache_key(root: &str) -> String {
    root.replace('/', "\\").to_lowercase()
}

#[cfg(not(windows))]
fn cache_key(root: &str) -> String {
    let trimmed = root.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

fn theme_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("theme.txt"))
}

fn load_theme_mode() -> Option<theme::Mode> {
    let path = theme_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    theme::Mode::parse(&text)
}

fn save_theme_mode(mode: theme::Mode) {
    let Some(path) = theme_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, mode.as_str());
}

/// The view to start in: an explicit DISK_SCANNER_VIEW wins (handy for
/// screenshots), then whatever was last used, then the sunburst.
fn initial_view() -> View {
    std::env::var("DISK_SCANNER_VIEW")
        .ok()
        .and_then(|value| View::parse(&value))
        .or_else(load_view)
        .unwrap_or(View::Rings)
}

fn view_path() -> Option<std::path::PathBuf> {
    Some(config_dir()?.join("view.txt"))
}

fn load_view() -> Option<View> {
    let text = std::fs::read_to_string(view_path()?).ok()?;
    View::parse(&text)
}

fn save_view(view: View) {
    let Some(path) = view_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, view.as_str());
}

impl Render for DiskApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let viewport_w = f32::from(viewport.width);
        let viewport_h = f32::from(viewport.height);
        // The volume list is painted by the chart canvas, which needs to know
        // where it must stop before it runs off the window.
        self.viewport = (viewport_w, viewport_h);
        // Switching volumes or folders swaps the chart under the pointer; a
        // hover index from the old chart must not survive into the new one.
        if let Some(chart) = &self.chart {
            if self.hover.is_some_and(|index| index >= chart.slices.len()) {
                self.hover = None;
                self.tooltip = None;
                self.menu = None;
                self.menu_hover = None;
            }
        }
        if !self.focus_handle.is_focused(window) {
            window.focus(&self.focus_handle);
        }

        let mut root = div()
            .id("app-root")
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .text_size(px(13.0))
            .font_family("Segoe UI")
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| this.on_key(event, cx)))
            .child(self.top_bar(cx));

        root = root
            .child({
                let focus = self.focus();
                let nav_w = if focus.is_some() {
                    NAV_W + GAP
                } else {
                    0.0
                };
                let mut row = div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .gap(px(GAP))
                    .px(px(PAD));
                // Same pane in every view: the volumes, then the open folder's
                // chain. It disappears only before the first tree exists.
                if let Some(focus) = &focus {
                    row = row.child(self.detail_nav(focus, window, cx));
                }
                row = row.child(self.chart_area(viewport_w, viewport_h, nav_w, window, cx));
                // The details table, the file list and the type totals already
                // carry their own numbers, so the breakdown panel would repeat
                // them. Before the first chart exists there is nothing to break
                // down either, and an empty column would push the startup card
                // off centre.
                if self.view.is_chart() && self.chart.is_some() {
                    row = row.child(self.panel(cx));
                }
                row
            })
            .child(self.status_bar(cx));

        for layer in self.overlays(cx) {
            root = root.child(layer);
        }
        root
    }
}

pub fn open(cx: &mut App) {
    let bounds = gpui::Bounds::centered(
        None,
        gpui::size(px(1320.0), px(880.0)),
        cx,
    );
    cx.open_window(
        WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(bounds)),
            window_min_size: Some(gpui::size(px(960.0), px(620.0))),
            titlebar: Some(gpui::TitlebarOptions {
                title: Some("Disk Scanner".into()),
                appears_transparent: false,
                traffic_light_position: None,
            }),
            ..Default::default()
        },
        |_window, cx| cx.new(DiskApp::new),
    )
    .expect("could not open the window");
    cx.activate(true);

    // The window exists by now, but give the compositor a moment before asking
    // for its handle, and try once more in case the first pass ran too early.
    crate::sys::apply_window_icon();
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(400))
            .await;
        crate::sys::apply_window_icon();
    })
    .detach();
}

#[allow(dead_code)]
const _: usize = MAX_RINGS;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, Node};

    fn folder(name: &str, path: &str, size: u64, children: Vec<Arc<Node>>) -> Arc<Node> {
        Arc::new(Node {
            name: name.to_string(),
            path: path.to_string(),
            size,
            files: 10,
            dirs: 2,
            kind: Kind::Dir,
            children,
        })
    }

    /// A small volume shaped like a system drive: nested folders whose sizes add
    /// up to their parent, which is what the sunburst relies on.
    fn sample_tree() -> Arc<Node> {
        folder(
            r"C:\",
            r"C:\",
            1000,
            vec![
                folder(
                    "Users",
                    r"C:\Users",
                    500,
                    vec![
                        folder(
                            "AppData",
                            r"C:\Users\AppData",
                            400,
                            vec![
                                folder("NVIDIA", r"C:\Users\AppData\NVIDIA", 300, Vec::new()),
                                folder("Temp", r"C:\Users\AppData\Temp", 100, Vec::new()),
                            ],
                        ),
                        folder("Docs", r"C:\Users\Docs", 100, Vec::new()),
                    ],
                ),
                folder(
                    "Program Files",
                    r"C:\Program Files",
                    300,
                    vec![folder("Apps", r"C:\Program Files\Apps", 300, Vec::new())],
                ),
                folder("Windows", r"C:\Windows", 200, Vec::new()),
            ],
        )
    }

    #[test]
    fn every_painted_arc_hit_tests_back_to_its_own_slice() {
        // Pointing at a wedge must name the folder that wedge is drawn for.
        // Getting this wrong is what made the ring highlight one folder while
        // the tooltip and the click talked about another.
        let chart = build_chart(sample_tree(), Metric::Bytes);
        let geom = Geom::new(700.0, chart.rings);
        assert!(chart.slices.len() >= 7, "the sample must draw several rings");

        for (index, slice) in chart.slices.iter().enumerate() {
            let (r0, r1) = geom.ring_bounds(slice.depth);
            let radius = (r0 + r1) * 0.5;
            let mid = slice.mid();
            let x = geom.cx + radius * mid.cos();
            let y = geom.cy + radius * mid.sin();
            assert_eq!(
                hit(&chart, &geom, x, y),
                Some(index),
                "the arc of {} must hit-test to itself",
                slice.node.name
            );
        }
    }

    #[test]
    fn the_hub_and_the_void_around_the_rings_hit_nothing() {
        let chart = build_chart(sample_tree(), Metric::Bytes);
        let geom = Geom::new(700.0, chart.rings);
        assert_eq!(hit(&chart, &geom, geom.cx, geom.cy), None);
        assert_eq!(
            hit(&chart, &geom, geom.cx, geom.cy + geom.radius() + 20.0),
            None
        );
    }

    #[test]
    fn a_layout_from_another_view_or_chart_is_not_trusted() {
        // The bug behind "hovering Program Files highlights something else":
        // the rings view kept answering hit tests with the treemap cells the
        // last tile frame had left in the shared layout.
        let chart = Arc::new(build_chart(sample_tree(), Metric::Bytes));
        let mut slot = Placed {
            origin: point(px(0.), px(0.)),
            geom: Geom::new(700.0, chart.rings),
            tiles: Arc::new(Vec::new()),
            view: View::Rings,
            chart: Some(chart.clone()),
            size: (700.0, 700.0),
        };
        assert!(slot.describes(View::Rings, &chart));

        // A treemap frame left behind must not answer for the ring.
        slot.view = View::Treemap;
        assert!(!slot.describes(View::Rings, &chart));

        // Nor may a chart from before a folder switch reuse the old geometry.
        slot.view = View::Rings;
        let other = Arc::new(build_chart(
            folder("Windows", r"C:\Windows", 200, Vec::new()),
            Metric::Bytes,
        ));
        assert!(!slot.describes(View::Rings, &other));
    }

    #[test]
    fn climbs_past_folded_chain_nodes_to_the_nearest_real_ancestor() {
        // The scanner folds `app.asar.unpacked/dsh/node_modules` into one node,
        // and the same prefix also exists as a small file beside it, so path
        // arithmetic resolves to the wrong node. "Up" must follow the rings.
        let sep = std::path::MAIN_SEPARATOR;
        let root_path = if cfg!(windows) {
            "C:\\".to_string()
        } else {
            "/".to_string()
        };
        let app_path = format!("{root_path}App");
        let resources_path = format!("{app_path}{sep}resources");
        let loose_path = format!("{resources_path}{sep}app.asar.unpacked");
        let chain_path = format!("{loose_path}{sep}dsh{sep}node_modules");

        let chain = folder(
            &format!("app.asar.unpacked{sep}dsh{sep}node_modules"),
            &chain_path,
            300,
            Vec::new(),
        );
        let loose_file = Arc::new(Node {
            name: "app.asar.unpacked".to_string(),
            path: loose_path.clone(),
            size: 1,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            children: Vec::new(),
        });
        let resources = folder(
            "resources",
            &resources_path,
            400,
            vec![chain.clone(), loose_file],
        );
        let app = folder("App", &app_path, 500, vec![resources.clone()]);
        let root = folder("root", &root_path, 500, vec![app.clone()]);

        assert_eq!(
            parent_of(&root, &chain.path)
                .expect("a ring above the chain")
                .path,
            resources_path
        );
        assert_eq!(
            parent_of(&root, &resources.path)
                .expect("a ring above resources")
                .path,
            app_path
        );
        assert_eq!(
            parent_of(&root, &app.path).expect("a ring above App").path,
            root_path
        );
        assert!(parent_of(&root, &root.path).is_none());

        // A segment pointing at the middle of the chain resolves to the chain
        // itself, not to a folder several levels below it: opening the chain is
        // what the segment names.
        let nested = folder(
            &format!("node_modules{sep}pkg"),
            &format!("{chain_path}{sep}pkg"),
            200,
            Vec::new(),
        );
        let chain = folder(
            &format!("app.asar.unpacked{sep}dsh{sep}node_modules"),
            &chain_path,
            300,
            vec![nested.clone()],
        );
        let loose_file = Arc::new(Node {
            name: "app.asar.unpacked".to_string(),
            path: loose_path.clone(),
            size: 1,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            children: Vec::new(),
        });
        let resources = folder(
            "resources",
            &resources_path,
            400,
            vec![chain.clone(), loose_file],
        );
        let app = folder("App", &app_path, 500, vec![resources.clone()]);
        let root = folder("root", &root_path, 500, vec![app.clone()]);

        assert_eq!(
            nearest_below(&root, &loose_path)
                .expect("the chain below the segment")
                .path,
            chain.path,
            "the chain stands for the segment, not the file of the same name"
        );
        assert_eq!(
            nearest_below(&root, &chain_path).expect("the chain matches itself").path,
            chain.path
        );
        assert!(
            nearest_below(&root, &format!("{chain_path}{sep}pkg{sep}gone")).is_none(),
            "a path nothing represents resolves to nothing"
        );
        // A file must never become the focus, however exactly it matches.
        assert!(find_dir(&root, &loose_path).is_none());
        assert_eq!(
            find_dir(&root, &chain_path).expect("a folder by its own path").path,
            chain.path
        );
    }

    #[test]
    fn the_navigation_tree_shows_only_open_folders() {
        let sep = std::path::MAIN_SEPARATOR;
        let root_path = if cfg!(windows) { "C:\\" } else { "/" };
        let users_path = format!("{root_path}Users");
        let bob_path = format!("{users_path}{sep}bob");
        let deep_path = format!("{bob_path}{sep}pictures");

        let deep = folder("pictures", &deep_path, 10, Vec::new());
        let bob = folder("bob", &bob_path, 10, vec![deep]);
        let users = folder("Users", &users_path, 10, vec![bob]);
        let file = Arc::new(Node {
            name: "readme.txt".to_string(),
            path: format!("{root_path}readme.txt"),
            size: 1,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            children: Vec::new(),
        });
        let root = folder("root", root_path, 21, vec![users, file]);

        let mut expanded = HashSet::new();
        // Nothing open but the root: only the root's folders are listed.
        reveal_path(&root, &deep_path, &mut expanded);
        let mut rows = Vec::new();
        push_nav_rows(&root, 0, &expanded, &mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row.node.name.as_str()).collect();
        assert_eq!(names, vec!["root", "Users", "bob", "pictures"], "files stay out of the tree");
        assert_eq!(rows[3].depth, 3, "each level indents once");
        assert!(!rows[3].expanded, "the focused folder itself is closed");

        // Closing `bob` hides everything below it.
        expanded.remove(&cache_key(&bob_path));
        let mut rows = Vec::new();
        push_nav_rows(&root, 0, &expanded, &mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row.node.name.as_str()).collect();
        assert_eq!(names, vec!["root", "Users", "bob"]);
        assert!(!rows[2].expanded);
        assert!(rows[2].has_children, "bob still offers its chevron");
    }

    #[test]
    fn a_volume_lists_its_folders_one_level_down() {
        let sep = std::path::MAIN_SEPARATOR;
        let root_path = if cfg!(windows) { "C:\\" } else { "/" };
        let users_path = format!("{root_path}Users");
        let bob_path = format!("{users_path}{sep}bob");

        let bob = folder("bob", &bob_path, 5, Vec::new());
        let users = folder("Users", &users_path, 5, vec![bob]);
        let file = Arc::new(Node {
            name: "readme.txt".to_string(),
            path: format!("{root_path}readme.txt"),
            size: 1,
            files: 1,
            dirs: 0,
            kind: Kind::File,
            children: Vec::new(),
        });
        let root = folder("C:\\", root_path, 11, vec![users, file]);

        // Closed volume: nothing below it.
        let mut expanded = HashSet::new();
        let mut rows = Vec::new();
        push_drive_rows(root_path, Some(&root), &expanded, &mut rows);
        assert!(rows.is_empty(), "a closed volume shows no folders");

        // Open volume: its folders sit one level in from the volume row, and
        // files stay out.
        expanded.insert(cache_key(root_path));
        let mut rows = Vec::new();
        push_drive_rows(root_path, Some(&root), &expanded, &mut rows);
        let names: Vec<&str> = rows.iter().map(|row| row.node.name.as_str()).collect();
        assert_eq!(names, vec!["Users"], "the volume row is drawn by the caller");
        assert_eq!(rows[0].depth, 1, "folders indent once under the volume");
        assert!(!rows[0].expanded, "an open volume does not open its folders");

        // A scan of a folder inside the volume is the only row it can show.
        let inside = format!("{root_path}Users");
        let sub = folder("Users", &inside, 5, Vec::new());
        let mut rows = Vec::new();
        push_drive_rows(root_path, Some(&sub), &expanded, &mut rows);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].node.path, inside);
        assert_eq!(rows[0].depth, 1);
    }

    #[test]
    fn revealing_a_folder_opens_its_ancestors_only() {
        let sep = std::path::MAIN_SEPARATOR;
        let root_path = if cfg!(windows) { "C:\\" } else { "/" };
        let a_path = format!("{root_path}a");
        let b_path = format!("{a_path}{sep}b");
        let other_path = format!("{root_path}other");
        let leaf = folder("b", &b_path, 1, Vec::new());
        let a = folder("a", &a_path, 1, vec![leaf]);
        let other = folder("other", &other_path, 1, Vec::new());
        let root = folder("root", root_path, 2, vec![a, other]);

        let mut expanded = HashSet::new();
        reveal_path(&root, &b_path, &mut expanded);

        assert!(expanded.contains(&cache_key(root_path)), "the root opens");
        assert!(expanded.contains(&cache_key(&a_path)), "the folder above opens");
        assert!(!expanded.contains(&cache_key(&b_path)), "the folder itself stays closed");
        assert!(!expanded.contains(&cache_key(&other_path)), "a sibling stays closed");
    }

    #[test]
    fn splices_a_refreshed_folder_and_moves_its_difference_up() {
        let users = folder("Users", r"C:\Users", 100, Vec::new());
        let windows = folder("Windows", r"C:\Windows", 50, Vec::new());
        let root = folder(
            "C:\\",
            r"C:\",
            150,
            vec![users.clone(), windows.clone()],
        );

        let refreshed = folder("Users", r"C:\Users", 140, Vec::new());
        let delta = Delta::between(&users, &refreshed);
        let merged = splice(&root, r"C:\Users", &refreshed, delta).expect("Users is in the tree");

        assert_eq!(merged.size, 190, "the volume total follows the delta");
        assert_eq!(merged.children[0].size, 140, "the folder is replaced");
        assert_eq!(merged.children[1].size, 50, "siblings are untouched");
        assert_eq!(merged.children.len(), 2);
    }

    #[test]
    fn splice_ignores_a_folder_that_is_not_in_the_tree() {
        let root = folder("C:\\", r"C:\", 150, vec![folder("Users", r"C:\Users", 100, Vec::new())]);
        let fresh = folder("Other", r"C:\Other", 5, Vec::new());
        assert!(splice(&root, r"C:\Other", &fresh, Delta::default()).is_none());
    }

    #[test]
    #[cfg(windows)]
    fn recognises_nested_paths() {
        assert!(is_inside(r"C:\", r"C:\Users"));
        assert!(is_inside(r"C:\Users", r"C:\Users\jcarillo"));
        assert!(!is_inside(r"C:\Users", r"C:\UsersX"));
        assert!(!is_inside(r"C:\Users", r"C:\Users"));
    }

    #[test]
    #[cfg(not(windows))]
    fn recognises_nested_paths() {
        assert!(is_inside("/", "/home"));
        assert!(is_inside("/home", "/home/bob"));
        assert!(!is_inside("/home", "/homebob"));
        assert!(!is_inside("/home", "/home"));
        assert!(!is_inside("/Home", "/home/bob"));
    }

    #[test]
    #[cfg(windows)]
    fn splits_a_folder_path_into_clickable_levels() {
        let crumbs = breadcrumb_segments(r"C:\Users\jcarillo\AppData\Local\Xamarin");
        let labels: Vec<_> = crumbs.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(
            labels,
            ["C:", "Users", "jcarillo", "AppData", "Local", "Xamarin"]
        );
        let paths: Vec<_> = crumbs.iter().map(|(_, path)| path.as_str()).collect();
        assert_eq!(
            paths,
            [
                r"C:\",
                r"C:\Users",
                r"C:\Users\jcarillo",
                r"C:\Users\jcarillo\AppData",
                r"C:\Users\jcarillo\AppData\Local",
                r"C:\Users\jcarillo\AppData\Local\Xamarin",
            ]
        );
    }

    #[test]
    #[cfg(windows)]
    fn keeps_a_drive_root_as_one_segment() {
        assert_eq!(breadcrumb_segments(r"C:\"), vec![("C:".into(), r"C:\".into())]);
        assert_eq!(breadcrumb_segments(r"C:"), vec![("C:".into(), r"C:\".into())]);
    }

    #[test]
    #[cfg(not(windows))]
    fn splits_a_folder_path_into_clickable_levels() {
        let crumbs = breadcrumb_segments("/home/bob/Pictures");
        let labels: Vec<_> = crumbs.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, ["/", "home", "bob", "Pictures"]);
        let paths: Vec<_> = crumbs.iter().map(|(_, path)| path.as_str()).collect();
        assert_eq!(
            paths,
            ["/", "/home", "/home/bob", "/home/bob/Pictures"]
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn the_root_is_one_segment() {
        assert_eq!(breadcrumb_segments("/"), vec![("/".into(), "/".into())]);
        assert_eq!(breadcrumb_segments(""), vec![("/".into(), "/".into())]);
    }

    #[test]
    #[cfg(windows)]
    fn leaves_unc_paths_alone() {
        let unc = r"\\server\share\folder";
        assert_eq!(breadcrumb_segments(unc), vec![(unc.into(), unc.into())]);
    }

    #[test]
    fn splits_a_file_into_name_and_folder() {
        assert_eq!(file_name(r"C:\Users\me\clip.mp4"), "clip.mp4");
        assert_eq!(file_folder(r"C:\Users\me\clip.mp4"), r"C:\Users\me");
        assert_eq!(file_name("/home/me/a.txt"), "a.txt");
        assert_eq!(file_folder("/home/me/a.txt"), "/home/me");
        // A file at the root of a volume has to keep the separator, or the
        // parent stops being a path at all.
        assert_eq!(file_folder(r"C:\a.txt"), r"C:\");
        assert_eq!(file_folder("/a.txt"), "/");
        assert_eq!(file_name("nameless"), "nameless");
        assert_eq!(file_folder("nameless"), "");
    }

    #[test]
    fn shortens_long_folders_from_the_front() {
        assert_eq!(short_folder(r"C:\short", 20), r"C:\short");
        let long = short_folder(r"C:\Users\me\Documents\repos\rust-explore\disk-scanner\crates", 20);
        assert_eq!(long.chars().count(), 20);
        assert!(long.starts_with('…'));
        assert!(long.ends_with("crates"));
    }

    #[test]
    fn quotes_csv_cells_only_when_needed() {
        assert_eq!(csv_cell("C:\\plain\\file.txt"), "C:\\plain\\file.txt");
        assert_eq!(csv_cell("a,b"), "\"a,b\"");
        assert_eq!(csv_cell("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_cell("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn writes_a_file_csv_with_a_header_and_one_row_per_file() {
        let dir = std::env::temp_dir().join(format!(
            "disk-scanner-csv-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).expect("temp folder");
        let path = dir.join("files.csv");
        let rows = vec![
            FileHit {
                path: r"C:\one\movie.mp4".to_string(),
                size: 2_000_000,
                modified: -1,
            },
            FileHit {
                path: r"C:\two\notes.txt".to_string(),
                size: 400,
                modified: 0,
            },
        ];
        write_file_csv(&path, &rows).expect("write the csv");
        let text = std::fs::read_to_string(&path).expect("read the csv back");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Name,Folder,Size,Modified,Kind");
        assert_eq!(lines[1], r"movie.mp4,C:\one,2000000,—,Video");
        assert!(lines[2].starts_with(r"notes.txt,C:\two,400,"));
        assert!(lines[2].ends_with(",Documents"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dates_from_the_scan_render_like_the_details_table() {
        assert_eq!(format_index_date(-1), "—");
        assert!(!format_index_date(0).is_empty());
    }

    #[test]
    fn deleting_a_copy_drops_it_from_its_set_and_forgets_a_lonely_copy() {
        let hit = |path: &str, size: u64| FileHit {
            path: path.to_string(),
            size,
            modified: -1,
        };
        let scan = DuplicateScan {
            groups: vec![
                DupGroup {
                    size: 3_000_000,
                    copies: vec![
                        hit(r"C:\photos\holiday.jpg", 3_000_000),
                        hit(r"C:\backup\holiday copy.jpg", 3_000_000),
                        hit(r"C:\backup\holiday again.jpg", 3_000_000),
                    ],
                },
                DupGroup {
                    size: 2_000_000,
                    copies: vec![
                        hit(r"C:\a\notes.pdf", 2_000_000),
                        hit(r"C:\b\notes.pdf", 2_000_000),
                    ],
                },
            ],
            hashed: 7,
            cancelled: false,
        };

        let pruned = prune_duplicate_copies(&scan, r"C:\backup");
        assert_eq!(pruned.hashed, 7);
        assert_eq!(pruned.groups.len(), 1, "the set under the deleted path is gone");
        assert_eq!(pruned.groups[0].copies.len(), 2);
        assert_eq!(pruned.groups[0].size, 2_000_000);

        // One copy left in its own set: nothing to compare any more.
        let pruned = prune_duplicate_copies(&scan, r"C:\a");
        assert_eq!(pruned.groups.len(), 1);
        assert_eq!(pruned.groups[0].copies.len(), 3);
    }
}
#[allow(dead_code)]
const _: f32 = TAU;

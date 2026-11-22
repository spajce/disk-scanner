//! The disk-scanner window: a sunburst you can zoom, plus a breakdown panel.

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use gpui::prelude::*;
use gpui::{
    canvas, div, linear_color_stop, linear_gradient, point, px, quad, relative, size,
    AnyElement, App, BorderStyle, Bounds,
    Context, Div, FocusHandle, FontWeight, Hsla, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, PathBuilder, Pixels, Point, Render, SharedString, Window,
    WindowOptions,
};

use disk_scan::{list_drives, scan_pausable, to_dto, DriveInfo, ScanCounters, ScanMeta};

use crate::model::{
    build_chart, heatmap, hit, log_rank, short_name, treemap, ChartModel, Geom, Kind, Metric, Node,
    Tile, View, MAX_RINGS, TREEMAP_HEADER,
};
use crate::theme;

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
const COL_SIZE: f32 = 92.0;
const COL_FILES: f32 = 70.0;
const COL_FOLDERS: f32 = 70.0;
const COL_STATUS: f32 = 66.0;
const COL_MODIFIED: f32 = 138.0;
const COL_TYPE: f32 = 136.0;
/// Two presses on the same row inside this window open it, like Explorer.
const DETAIL_DOUBLE_CLICK: Duration = Duration::from_millis(450);

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
    elapsed: f64,
    /// Wall-clock time of the walk, so an entry restored from disk can say how
    /// old it is as well as one from earlier in this session.
    at: SystemTime,
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
    /// Modification dates for the rows on screen, collected on a worker thread.
    detail_meta: Arc<Mutex<HashMap<String, DetailMeta>>>,
    /// Folder whose dates have already been requested, so a frame never starts
    /// the same walk twice.
    detail_meta_for: Option<String>,
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
            detail_meta: Arc::new(Mutex::new(HashMap::new())),
            detail_meta_for: None,
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
            .find_map(|candidate| Node::find(&tree, candidate))
        {
            self.set_focus(node, cx);
            return;
        }
        // A breadcrumb segment can name the middle of a folded chain, where the
        // only real node is the chain itself (`a\b\c`).
        if let Some(node) = deepest_inside(&tree, &path) {
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
                let elapsed = cached.elapsed;
                let cached_at = cached.at;
                if let Some(job) = &self.job {
                    job.abort();
                }
                self.job = None;
                self.generation += 1;
                let same_root = self.last_root.eq_ignore_ascii_case(&root);
                let focus = if same_root && !self.focus_path.is_empty() {
                    Node::find(&tree, &self.focus_path).unwrap_or_else(|| tree.clone())
                } else {
                    tree.clone()
                };
                self.tree = Some(tree.clone());
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

        let same_root = self.last_root.eq_ignore_ascii_case(&target);
        self.last_root = target.clone();
        save_last_root(&target);

        // A drive we have already walked is painted from memory straight away;
        // the scan then runs behind it and refreshes the numbers in place.
        if let Some(cached) = self.cache.get(&cache_key(&target)) {
            let tree = cached.tree.clone();
            let focus = if same_root && !self.focus_path.is_empty() {
                Node::find(&tree, &self.focus_path).unwrap_or_else(|| tree.clone())
            } else {
                tree.clone()
            };
            self.focus_path = focus.path.clone();
            self.show_chart(focus);
            self.tree = Some(tree);
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
                match scan_pausable(Path::new(&walk_root), &cancel, &pause, &counters) {
                    Ok(raw) => {
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
                                let _ = disk_scan::cache::save(dir, &meta, &dto);
                            }
                        }
                        let tree = Node::from_dto(dto);
                        let cancelled = cancel.load(Ordering::Relaxed);
                        Ok((tree, elapsed, cancelled))
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
        result: Result<(Arc<Node>, f64, bool), String>,
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
            Ok((tree, elapsed, cancelled)) => {
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
                        self.cache.insert(
                            cache_key(&self.last_root),
                            Cached {
                                tree: merged.clone(),
                                elapsed,
                                at: SystemTime::now(),
                            },
                        );
                        self.tree = Some(merged);
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
                self.cache.insert(
                    cache_key(&self.last_root),
                    Cached {
                        tree: tree.clone(),
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
        cx.notify();
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
    fn detail_nav(&self, focus: &Arc<Node>, cx: &mut Context<Self>) -> Div {
        // The rows inside the pane scroll both ways: a deep folder chain is
        // indented past the right edge, and a machine with several volumes runs
        // past the bottom. Neither should be cut off.
        let mut pane = div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(8.0))
            .child(section_label("THIS PC"));

        let active_root = self.active_drive().map(|drive| drive.root.clone());
        for drive in &self.drives {
            let active = active_root
                .as_deref()
                .is_some_and(|root| root.eq_ignore_ascii_case(&drive.root));
            let fraction = usage_fraction(drive);
            let root = drive.root.clone();
            let title = drive_title(drive);
            let usage = format!(
                "{} free of {}",
                theme::format_bytes(drive.free),
                theme::format_bytes(drive.total)
            );
            pane = pane.child(
                div()
                    .id(SharedString::from(format!("nav-{root}")))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .px(px(7.0))
                    .py(px(5.0))
                    .rounded(px(7.0))
                    .cursor_pointer()
                    .bg(if active {
                        theme::accent_soft()
                    } else {
                        theme::surface()
                    })
                    .hover(|style| style.bg(theme::surface_alt()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.last_root.eq_ignore_ascii_case(&root) {
                            this.switch_to(root.clone(), cx);
                        }
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(div().text_size(px(11.0)).child("💾"))
                            .child(div().flex_shrink_0().whitespace_nowrap().child(title)),
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
                                    .bg(if fraction > 0.9 {
                                        theme::danger()
                                    } else {
                                        theme::accent()
                                    })
                                    // A fraction of the row rather than a fixed
                                    // width, so the bar stays honest when a
                                    // long name has widened the pane.
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
            );
        }

        pane = pane.child(
            div()
                .mt(px(8.0))
                .child(section_label("CURRENT FOLDER")),
        );
        let segments = breadcrumb_segments(&focus.path);
        let last = segments.len().saturating_sub(1);
        for (index, (label, full)) in segments.into_iter().enumerate() {
            let current = index == last;
            let target = full;
            pane = pane.child(
                div()
                    .id(SharedString::from(format!("nav-crumb-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .pl(px(7.0 + index as f32 * 10.0))
                    .pr(px(6.0))
                    .py(px(3.0))
                    .rounded(px(5.0))
                    .cursor_pointer()
                    .text_color(if current {
                        theme::text()
                    } else {
                        theme::text_dim()
                    })
                    .font_weight(if current {
                        FontWeight::MEDIUM
                    } else {
                        FontWeight::NORMAL
                    })
                    .hover(|style| style.bg(theme::surface_alt()).text_color(theme::text()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.jump_to(target.clone(), cx);
                    }))
                    .child(
                        div()
                            .text_size(px(9.0))
                            .opacity(0.6)
                            .child(if current { "▸" } else { "·" }),
                    )
                    .child(div().flex_shrink_0().whitespace_nowrap().child(label)),
            );
        }

        // The frame stays the width the layout reserves; only its contents move.
        div()
            .w(px(NAV_W))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::hairline())
            .text_size(px(11.5))
            .overflow_hidden()
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
                    .overflow_scroll()
                    .child(pane.flex_shrink_0().min_w(px(NAV_W - 2.0))),
            )
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
            .child(body)
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
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.detail_header_cell("Name", DetailKey::Name, false, cx)),
            )
            .child(
                self.detail_header_cell("Size", DetailKey::Size, true, cx)
                    .w(px(COL_SIZE))
                    .flex_shrink_0(),
            )
            .child(
                self.detail_header_cell("Files", DetailKey::Files, true, cx)
                    .w(px(COL_FILES))
                    .flex_shrink_0(),
            )
            .child(
                self.detail_header_cell("Folders", DetailKey::Folders, true, cx)
                    .w(px(COL_FOLDERS))
                    .flex_shrink_0(),
            )
            .child(
                self.detail_header_cell("Status", DetailKey::Status, false, cx)
                    .w(px(COL_STATUS))
                    .flex_shrink_0(),
            )
            .child(
                self.detail_header_cell("Date Modified", DetailKey::Modified, false, cx)
                    .w(px(COL_MODIFIED))
                    .flex_shrink_0(),
            )
            .child(
                self.detail_header_cell("Type", DetailKey::Type, false, cx)
                    .w(px(COL_TYPE))
                    .flex_shrink_0(),
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
            .child(
                div()
                    .flex_1()
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
            .child(detail_cell(size, COL_SIZE, true))
            .child(detail_cell(theme::format_count(node.files), COL_FILES, true))
            .child(detail_cell(theme::format_count(node.dirs), COL_FOLDERS, true))
            .child(detail_cell(status.to_string(), COL_STATUS, false))
            .child(detail_cell(date, COL_MODIFIED, false))
            .child(detail_cell(kind, COL_TYPE, false))
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
                    .child(self.view_button(View::Details, cx)),
            );
        // The metric toggle re-proportions the chart. The details table shows a
        // size and a file count in its own columns, so the buttons mean nothing
        // while it is open.
        if self.view != View::Details {
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
                if self.menu.is_some() {
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
            // 1 / 2 / 3 / 4 pick the visualisation, like the toolbar buttons.
            "1" => self.set_view(View::Rings, cx),
            "2" => self.set_view(View::Treemap, cx),
            "3" => self.set_view(View::Heatmap, cx),
            "4" => self.set_view(View::Details, cx),
            // In the details view Enter opens the selected folder, as Explorer
            // does; the other views have no selection to act on.
            "enter" => {
                if self.view == View::Details {
                    if let Some(node) = self.selected_detail_node() {
                        self.detail_open(node, cx);
                    }
                }
            }
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

    fn status_bar(&self) -> Div {
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
            .child(div().truncate().child(right))
    }

    fn overlays(&self, _cx: &mut Context<Self>) -> Vec<AnyElement> {
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
fn detail_cell(text: String, width: f32, right: bool) -> Div {    let mut cell = div()
        .w(px(width))
        .flex_shrink_0()
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

/// The deepest tree node that lives *below* `path`, used when a breadcrumb
/// segment names the middle of a folded chain and no node carries that exact
/// path.
fn deepest_inside(node: &Arc<Node>, path: &str) -> Option<Arc<Node>> {
    let mut deepest: Option<Arc<Node>> = None;
    for child in &node.children {
        if is_inside(&child.path, path) {
            // The segment is deeper than this child: keep walking down to it.
            if let Some(found) = deepest_inside(child, path) {
                keep_deeper(&mut deepest, found);
            }
        } else if is_inside(path, &child.path) {
            // The child (or something under it) is the chain below the segment.
            let found = deepest_inside(child, path).unwrap_or_else(|| child.clone());
            keep_deeper(&mut deepest, found);
        }
    }
    deepest
}

fn keep_deeper(best: &mut Option<Arc<Node>>, candidate: Arc<Node>) {
    if best
        .as_ref()
        .is_none_or(|current| candidate.path.len() > current.path.len())
    {
        *best = Some(candidate);
    }
}

/// True when `child` sits somewhere below `parent` (`C:\` contains `C:\Users`).
/// Whether `child` lives inside `parent`. The comparison is case-insensitive on
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
                    row = row.child(self.detail_nav(focus, cx));
                }
                row = row.child(self.chart_area(viewport_w, viewport_h, nav_w, window, cx));
                // The details table already lists the children with their
                // numbers, so the breakdown panel would repeat itself. Before
                // the first chart exists there is nothing to break down either,
                // and an empty column would push the startup card off centre.
                if self.view != View::Details && self.chart.is_some() {
                    row = row.child(self.panel(cx));
                }
                row
            })
            .child(self.status_bar());

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

        // A breadcrumb segment pointing at the middle of the chain still lands
        // on the deepest node that lives under it.
        assert_eq!(
            deepest_inside(&root, &loose_path)
                .expect("the chain below the segment")
                .path,
            chain.path
        );
        assert!(deepest_inside(&root, &chain.path).is_none());
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
}
#[allow(dead_code)]
const _: f32 = TAU;

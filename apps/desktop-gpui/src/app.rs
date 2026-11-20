//! The disk-scanner window: a sunburst you can zoom, plus a breakdown panel.

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    canvas, div, linear_color_stop, linear_gradient, point, px, quad, size,
    AnyElement, App, BorderStyle, Bounds,
    Context, Div, FocusHandle, FontWeight, Hsla, IntoElement, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, PathBuilder, Pixels, Point, Render, SharedString, Window,
    WindowOptions,
};

use disk_scan::{list_drives, scan, to_dto, DriveInfo, ScanCounters};

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

struct ScanJob {
    cancel: Arc<AtomicBool>,
    generation: u64,
}

enum Phase {
    Empty,
    Scanning,
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

struct Placed {
    origin: Point<Pixels>,
    geom: Geom,
    /// Treemap / heatmap cells for the frame on screen, indexed like
    /// `ChartModel::slices`. Empty while the sunburst is showing.
    tiles: Arc<Vec<Tile>>,
    view: View,
    /// Logical size of the drawing area measured during prepaint.
    size: (f32, f32),
}

/// A finished scan kept around so switching back to a drive is instant.
struct Cached {
    tree: Arc<Node>,
    elapsed: f64,
    at: Instant,
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
            size: (0.0, 0.0),
        }));
        let theme_mode = load_theme_mode().unwrap_or(theme::Mode::System);
        let system_dark = !crate::sys::system_prefers_light();
        theme::apply(theme_mode, system_dark);
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
            cache: HashMap::new(),
            theme_mode,
            system_dark,
            view: initial_view(),
            tile_size: (-1.0, -1.0),
        };

        // Follow the OS while the skin is on "Auto": the registry value changes
        // when the user flips Windows between light and dark.
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

    fn set_focus(&mut self, node: Arc<Node>, cx: &mut Context<Self>) {
        self.focus_path = node.path.clone();
        self.chart = Some(Arc::new(build_chart(node, self.metric)));
        self.hover = None;
        self.tooltip = None;
        self.menu = None;
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
        if std::path::Path::new(&path).is_dir() {
            self.switch_to(path, cx);
        } else {
            self.note = Some(format!("{} is not in this scan", short_name(&path, 44)));
            cx.notify();
        }
    }

    fn go_up(&mut self, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let current = self.focus_path.clone();
        let parent = Path::new(&current)
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
            .filter(|parent| !parent.is_empty());
        match parent {
            Some(parent) => {
                if let Some(node) = Node::find(&tree, &parent) {
                    self.set_focus(node, cx);
                }
            }
            None => self.set_focus(tree, cx),
        }
    }

    fn set_metric(&mut self, metric: Metric, cx: &mut Context<Self>) {
        if self.metric == metric {
            return;
        }
        self.metric = metric;
        if let Some(focus) = self.focus() {
            self.chart = Some(Arc::new(build_chart(focus, metric)));
        }
        self.hover = None;
        self.tooltip = None;
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
                if let Some(job) = &self.job {
                    job.cancel.store(true, Ordering::Relaxed);
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
                self.chart = Some(Arc::new(build_chart(focus, self.metric)));
                self.phase = Phase::Ready {
                    elapsed,
                    files: tree.files,
                    bytes: tree.size,
                };
                self.last_root = root.clone();
                save_last_root(&root);
                self.hover = None;
                self.tooltip = None;
                self.menu = None;
                self.note = Some(format!(
                    "Showing the scan from {} ago — Rescan to refresh",
                    theme::format_seconds(cached.at.elapsed().as_secs_f64())
                ));
                cx.notify();
                return;
        }
        self.start_scan(root, cx);
    }

    /// Rings, treemap or heatmap over the same tree.
    fn set_view(&mut self, view: View, cx: &mut Context<Self>) {
        if self.view == view {
            return;
        }
        self.view = view;
        save_view(view);
        self.hover = None;
        self.tooltip = None;
        cx.notify();
    }

    fn set_theme_mode(&mut self, mode: theme::Mode, cx: &mut Context<Self>) {        self.theme_mode = mode;
        theme::apply(mode, self.system_dark);
        save_theme_mode(mode);
        cx.notify();
    }

    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Relaxed);
        }
        if self.job.is_none() && !matches!(self.phase, Phase::Scanning) {
            return;
        }
        self.job = None;
        self.generation += 1;
        self.phase = match self.cache.get(&cache_key(&self.last_root)) {
            Some(cached) => Phase::Ready {
                elapsed: cached.elapsed,
                files: cached.tree.files,
                bytes: cached.tree.size,
            },
            None if self.chart.is_some() => Phase::Ready {
                elapsed: 0.0,
                files: 0,
                bytes: 0,
            },
            None => Phase::Empty,
        };
        self.note = Some("Scan stopped".to_string());
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
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.generation += 1;
        let generation = self.generation;
        let cancel = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(ScanCounters::new());
        self.job = Some(ScanJob {
            cancel: cancel.clone(),
            generation,
        });
        self.phase = Phase::Scanning;
        self.progress = (0, 0, target.clone());
        self.hover = None;
        self.tooltip = None;
        self.menu = None;
        self.note = None;

        if subtree {
            // Keep the chart and the volume totals; only the pill changes.
            cx.notify();
            self.spawn_scan(target, subtree, generation, cancel, counters, cx);
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
            self.chart = Some(Arc::new(build_chart(focus, self.metric)));
            self.tree = Some(tree);
        }
        cx.notify();
        self.spawn_scan(target, subtree, generation, cancel, counters, cx);
    }

    fn spawn_scan(
        &mut self,
        target: String,
        subtree: bool,
        generation: u64,
        cancel: Arc<AtomicBool>,
        counters: Arc<ScanCounters>,
        cx: &mut Context<Self>,
    ) {
        // Progress: a cheap snapshot every 120 ms, so the UI stays live while
        // the walker chews through millions of files.
        let tick_counters = counters.clone();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(120))
                    .await;
                let keep_going = this.update(cx, |this, cx| {
                    if this.job.as_ref().map(|job| job.generation) != Some(generation) {
                        return false;
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

        // The walk itself runs on a background thread; the DTO conversion and
        // the Arc tree are built there too.
        let walk_root = target.clone();
        cx.spawn(async move |this, cx| {
            let work = cx.background_spawn(async move {
                let started = Instant::now();
                match scan(Path::new(&walk_root), &cancel, &counters) {
                    Ok(raw) => {
                        let total = raw.size;
                        let dto = to_dto(&raw, total);
                        let tree = Node::from_dto(&dto);
                        let cancelled = cancel.load(Ordering::Relaxed);
                        Ok((tree, started.elapsed().as_secs_f64(), cancelled))
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
        self.job = None;
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
                        self.chart = Some(Arc::new(build_chart(focus, self.metric)));
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
                                at: Instant::now(),
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
                self.chart = Some(Arc::new(build_chart(tree.clone(), self.metric)));
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
                        at: Instant::now(),
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
        self.chart = Some(Arc::new(build_chart(node, self.metric)));
        self.hover = None;
        self.tooltip = None;
        self.menu = None;
        cx.notify();
    }

    // ---------------------------------------------------------------- render

    fn top_bar(&self, cx: &mut Context<Self>) -> Div {
        let mut chips: Vec<AnyElement> = Vec::new();
        for (index, drive) in self.drives.iter().enumerate() {
            let active = self.last_root.eq_ignore_ascii_case(&drive.root);
            chips.push(self.drive_chip(index, drive, active, cx).into_any_element());
        }

        div()
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
            .children(chips)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .child(self.view_button(View::Rings, cx))
                    .child(self.view_button(View::Treemap, cx))
                    .child(self.view_button(View::Heatmap, cx)),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(self.metric_button("Size", Metric::Bytes, cx))
                    .child(self.metric_button("Files", Metric::Files, cx)),
            )
            .child(self.theme_button(cx))
            .child(self.action_button(self.rescan_label(), cx, |this, cx| {
                this.rescan_focus(cx);
            }))
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

    fn drive_chip(
        &self,
        index: usize,
        drive: &DriveInfo,
        active: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let used = drive.total.saturating_sub(drive.free);
        let fraction = if drive.total == 0 {
            0.0
        } else {
            used as f32 / drive.total as f32
        };
        let root = drive.root.clone();
        let label = if drive.label.is_empty() {
            drive.root.trim_end_matches('\\').to_string()
        } else {
            format!("{}  {}", drive.root.trim_end_matches('\\'), drive.label)
        };

        div()
            .id(SharedString::from(format!("drive-{index}")))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(9.0))
            .bg(if active {
                theme::accent_soft()
            } else {
                theme::surface()
            })
            .border_1()
            .border_color(if active { theme::accent() } else { theme::hairline() })
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_alt()))
            .on_click(cx.listener(move |this, _, _, cx| {
                // Clicking a drive only *shows* it: a cached scan paints
                // instantly and is not re-walked. The Rescan button is the
                // explicit way to refresh.
                if !this.last_root.eq_ignore_ascii_case(&root) {
                    this.switch_to(root.clone(), cx);
                }
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .text_size(px(11.5))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(label),
                    )
                    .child(
                        div()
                            .text_size(px(10.5))
                            .text_color(theme::text_faint())
                            .child(format!(
                                "{} free of {}",
                                theme::format_bytes(drive.free),
                                theme::format_bytes(drive.total)
                            )),
                    ),
            )
            .child(
                div()
                    .h(px(3.0))
                    .w(px(150.0))
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
                            .w(px(150.0 * fraction)),
                    ),
            )
    }

    fn view_button(&self, view: View, cx: &mut Context<Self>) -> gpui::Stateful<Div> {
        let active = self.view == view;
        let glyph = match view {
            View::Rings => "◎",
            View::Treemap => "▦",
            View::Heatmap => "▩",
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

    fn chart_area(
        &mut self,
        viewport_w: f32,
        viewport_h: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        let content_w = viewport_w - PAD * 2.0;
        let content_h = viewport_h - TOPBAR_H - STATUSBAR_H - PAD * 2.0;
        let available_w = content_w - PANEL_W - GAP;
        let side = available_w.min(content_h).max(240.0) - 4.0;

        let scanning = matches!(self.phase, Phase::Scanning);

        let Some(chart) = self.chart.clone() else {
            return div()
                .relative()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .justify_center()
                .child(self.scan_card(cx));
        };

        if self.view != View::Rings {
            return self.tiles_area(&chart, scanning, window, cx);
        }

        let rings = chart.rings;
        let geom = Geom::new(side, rings);
        let placed = self.placed.clone();
        if let Ok(mut slot) = placed.lock() {
            slot.geom = geom;
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
        } else {
            format!("{} · {}", self.metric.unit(), format!("{} rings", chart.rings))
        };

        let side_px = side;
        let rings = chart.rings;
        let store_prepaint = placed.clone();
        let chart_paint = chart.clone();
        let store_paint = placed.clone();
        let menu_paint = self.menu.clone();
        let menu_hover = self.menu_hover;
        let mut area = div()
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
                    )
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
        }
        area
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
        // line up with the cells painted underneath them.
        let measured = self
            .placed
            .lock()
            .map(|slot| slot.size)
            .unwrap_or((0.0, 0.0));
        let tiles: Arc<Vec<Tile>> = if measured.0 > 1.0 && measured.1 > 1.0 {
            Arc::new(match view {
                View::Heatmap => heatmap(chart, measured.0, measured.1),
                _ => treemap(chart, measured.0, measured.1),
            })
        } else {
            if measured != self.tile_size {
                self.tile_size = measured;
                window.request_animation_frame();
            }
            Arc::new(Vec::new())
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
                    .child(self.action_button("Stop", cx, |this, cx| this.cancel_scan(cx))),
            )
    }

    /// The full-card placeholder used before any scan has produced a chart.
    fn scan_card(&self, cx: &mut Context<Self>) -> Div {
        let (files, bytes, path) = &self.progress;
        let scanning = matches!(self.phase, Phase::Scanning);
        let title = match &self.phase {
            Phase::Scanning => format!("Scanning {}…", self.last_root),
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
        if scanning {
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
                )
                .child(self.action_button("Stop", cx, |this, cx| this.cancel_scan(cx)));
        }
        card
    }

    fn pick(&self, position: Point<Pixels>) -> Option<(usize, f32, f32)> {
        let chart = self.chart.as_ref()?;
        let slot = self.placed.lock().ok()?;
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
        Some((index, f32::from(position.x), f32::from(position.y)))
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
            let node = chart.slices[index].node.clone();
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
                let node = chart.slices[index].node.clone();
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
                    self.cancel_scan(cx);
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
            // 1 / 2 / 3 pick the visualisation, like the toolbar buttons.
            "1" => self.set_view(View::Rings, cx),
            "2" => self.set_view(View::Treemap, cx),
            "3" => self.set_view(View::Heatmap, cx),
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
        let of_total = if chart.total > 0.0 {
            metric.weight(&focus) / chart.total
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
            let hue = chart
                .slices
                .iter()
                .find(|slice| Arc::ptr_eq(&slice.node, child))
                .map(|slice| slice.hue)
                .unwrap_or(206.0);
            let color = if child.kind == Kind::Other {
                theme::rest_color(1)
            } else {
                theme::slice_color(hue, 1)
            };
            let is_hover = self
                .hover
                .and_then(|hover| chart.slices.get(hover))
                .is_some_and(|slice| Arc::ptr_eq(&slice.node, child));

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
            .child(
                div()
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
                            .justify_between()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(focus.name.clone()),
                            )
                            .child(self.action_button("Up", cx, |this, cx| this.go_up(cx))),
                    )
                    .child(self.path_crumbs(&focus.path, cx))
                    .child(
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
                        div().mt(px(8.0)).flex().gap(px(6.0)).child(self.action_button(
                            "Reveal in Explorer",
                            cx,
                            |this, _| {
                                if let Some(focus) = this.focus() {
                                    crate::sys::reveal(&focus.path, focus.is_dir());
                                }
                            },
                        )),
                    ),
            )
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
        let (left, right) = match &self.phase {
            Phase::Empty => ("Ready".to_string(), String::new()),
            Phase::Scanning => {
                let (files, bytes, path) = &self.progress;
                (
                    format!(
                        "Scanning… {} files · {} · {}",
                        theme::format_count(*files),
                        theme::format_bytes(*bytes),
                        short_name(path, 46)
                    ),
                    "Esc / hub click to stop".to_string(),
                )
            }
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
                let x = tooltip.x.min(f32::MAX);
                layers.push(
                    div()
                        .absolute()
                        .left(px(x + 14.0))
                        .top(px(tooltip.y + 16.0))
                        .w(px(280.0))
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
                                    "{} · {} of {} · {} files",
                                    theme::format_bytes(node.size),
                                    theme::format_percent(share),
                                    match chart.metric {
                                        Metric::Bytes => "folder",
                                        Metric::Files => "parent",
                                    },
                                    theme::format_count(node.files)
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

fn ancestors(chart: &ChartModel, index: usize) -> HashSet<usize> {    let mut set = HashSet::new();
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

/// True when `child` sits somewhere below `parent` (`C:\` contains `C:\Users`).
fn is_inside(parent: &str, child: &str) -> bool {
    if parent.len() >= child.len() {
        return false;
    }
    let (parent_bytes, child_bytes) = (parent.as_bytes(), child.as_bytes());
    if !child_bytes[..parent_bytes.len()].eq_ignore_ascii_case(parent_bytes) {
        return false;
    }
    parent.ends_with('\\') || child_bytes[parent_bytes.len()] == b'\\'
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

fn last_root_path() -> Option<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        std::path::PathBuf::from(appdata)
            .join("disk-scanner")
            .join("last-root.txt"),
    )
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

fn cache_key(root: &str) -> String {
    root.replace('/', "\\").to_lowercase()
}

fn theme_path() -> Option<std::path::PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        std::path::PathBuf::from(appdata)
            .join("disk-scanner")
            .join("theme.txt"),
    )
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
    let mut path = std::env::var_os("APPDATA").map(std::path::PathBuf::from)?;
    path.push("disk-scanner");
    path.push("view.txt");
    Some(path)
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
            .child(self.top_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .gap(px(GAP))
                    .px(px(PAD))
                    .child(self.chart_area(viewport_w, viewport_h, window, cx))
                    .child(self.panel(cx)),
            )
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
    fn recognises_nested_paths() {
        assert!(is_inside(r"C:\", r"C:\Users"));
        assert!(is_inside(r"C:\Users", r"C:\Users\jcarillo"));
        assert!(!is_inside(r"C:\Users", r"C:\UsersX"));
        assert!(!is_inside(r"C:\Users", r"C:\Users"));
    }

    #[test]
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
    fn keeps_a_drive_root_as_one_segment() {
        assert_eq!(breadcrumb_segments(r"C:\"), vec![("C:".into(), r"C:\".into())]);
        assert_eq!(breadcrumb_segments(r"C:"), vec![("C:".into(), r"C:\".into())]);
    }

    #[test]
    fn leaves_unc_paths_alone() {
        let unc = r"\\server\share\folder";
        assert_eq!(breadcrumb_segments(unc), vec![(unc.into(), unc.into())]);
    }
}
#[allow(dead_code)]
const _: f32 = TAU;

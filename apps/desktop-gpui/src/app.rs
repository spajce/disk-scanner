//! The disk-scanner window: a sunburst you can zoom, plus a breakdown panel.

use std::collections::HashSet;
use std::f32::consts::TAU;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    canvas, div, point, px, AnyElement, App, Context, Div, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, PathBuilder, Pixels, Point, Render,
    SharedString, Window, WindowOptions,
};

use disk_scan::{list_drives, scan, to_dto, DriveInfo, ScanCounters};

use crate::model::{build_chart, hit, short_name, ChartModel, Geom, Kind, Metric, Node, MAX_RINGS};
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

struct Menu {
    x: f32,
    y: f32,
    node: Arc<Node>,
}

struct Confirm {
    node: Arc<Node>,
}

struct Placed {
    origin: Point<Pixels>,
    geom: Geom,
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
    confirm: Option<Confirm>,
    placed: Arc<Mutex<Placed>>,
    last_root: String,
    note: Option<String>,
    focus_handle: FocusHandle,
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
        }));
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
            confirm: None,
            placed,
            last_root: last_root.clone(),
            note: None,
            focus_handle: cx.focus_handle(),
        };
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

    fn start_scan(&mut self, root: String, cx: &mut Context<Self>) {
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
        self.progress = (0, 0, root.clone());
        self.hover = None;
        self.tooltip = None;
        self.menu = None;
        self.confirm = None;
        self.note = None;
        self.last_root = root.clone();
        save_last_root(&root);
        cx.notify();

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
        cx.spawn(async move |this, cx| {
            let work = cx.background_spawn(async move {
                let started = Instant::now();
                match scan(Path::new(&root), &cancel, &counters) {
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
            this.update(cx, |this, cx| this.finish_scan(generation, result, cx))
                .ok();
        })
        .detach();
    }

    fn finish_scan(
        &mut self,
        generation: u64,
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
                    self.phase = Phase::Empty;
                    cx.notify();
                    return;
                }
                self.focus_path = tree.path.clone();
                self.chart = Some(Arc::new(build_chart(tree.clone(), self.metric)));
                self.phase = Phase::Ready {
                    elapsed,
                    files: tree.files,
                    bytes: tree.size,
                };
                self.tree = Some(tree);
            }
            Err(error) => self.phase = Phase::Failed(error),
        }
        cx.notify();
    }

    fn ask_delete(&mut self, node: Arc<Node>, cx: &mut Context<Self>) {
        if node.kind == Kind::Other || node.path.is_empty() {
            return;
        }
        self.confirm = Some(Confirm { node });
        self.menu = None;
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let node = confirm.node;
        let path = node.path.clone();
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_spawn(async move { crate::sys::recycle(&path) })
                .await;
            this.update(cx, |this, cx| match outcome {
                Ok(()) => {
                    this.remove_subtree(&node.path, cx);
                    this.note = Some(format!("Moved {} to the Recycle Bin", node.name));
                }
                Err(error) => this.note = Some(error),
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn remove_subtree(&mut self, target: &str, cx: &mut Context<Self>) {
        let Some(tree) = self.tree.clone() else { return };
        let pruned = prune(&tree, target);
        let mut focus = self.focus_path.clone();
        if focus.to_lowercase().starts_with(&target.to_lowercase()) {
            focus = tree.path.clone();
        }
        self.tree = Some(pruned.clone());
        self.focus_path = focus;
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
                    .gap(px(6.0))
                    .child(self.metric_button("Size", Metric::Bytes, cx))
                    .child(self.metric_button("Files", Metric::Files, cx)),
            )
            .child(self.action_button("Rescan", cx, |this, cx| {
                let root = this.last_root.clone();
                this.start_scan(root, cx);
            }))
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
                this.start_scan(root.clone(), cx);
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

    fn chart_area(&mut self, viewport_w: f32, viewport_h: f32, cx: &mut Context<Self>) -> Div {
        let content_w = viewport_w - PAD * 2.0;
        let content_h = viewport_h - TOPBAR_H - STATUSBAR_H - PAD * 2.0;
        let available_w = content_w - PANEL_W - GAP;
        let side = available_w.min(content_h).max(240.0) - 4.0;

        let Some(chart) = self.chart.clone() else {
            return div().flex_1().flex().items_center().justify_center().child(
                div()
                    .text_color(theme::text_faint())
                    .child("Waiting for the first scan…"),
            );
        };

        let rings = chart.rings;
        let geom = Geom::new(side, rings);
        let placed = self.placed.clone();
        if let Ok(mut slot) = placed.lock() {
            slot.geom = geom;
        }

        let hover = self.hover;
        let chain = hover.map(|index| ancestors(&chart, index)).unwrap_or_default();

        let mut labels: Vec<AnyElement> = Vec::new();
        for (index, slice) in chart.slices.iter().enumerate() {
            if slice.depth > 3 {
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
        div()
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
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.on_chart_right_click(event, cx);
                        }),
                    ),
            )
    }

    fn pick(&self, position: Point<Pixels>) -> Option<(usize, f32, f32)> {
        let chart = self.chart.as_ref()?;
        let slot = self.placed.lock().ok()?;
        let local_x = f32::from(position.x - slot.origin.x);
        let local_y = f32::from(position.y - slot.origin.y);
        let index = hit(chart, &slot.geom, local_x, local_y)?;
        Some((index, f32::from(position.x), f32::from(position.y)))
    }

    fn on_chart_move(&mut self, event: &MouseMoveEvent, cx: &mut Context<Self>) {
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
        let Some(chart) = self.chart.clone() else {
            return;
        };
        // A click inside the hub walks back up.
        let in_hub = match self.placed.lock() {
            Ok(slot) => {
                let local_x = f32::from(event.position.x - slot.origin.x);
                let local_y = f32::from(event.position.y - slot.origin.y);
                let dx = local_x - slot.geom.cx;
                let dy = local_y - slot.geom.cy;
                (dx * dx + dy * dy).sqrt() < slot.geom.inner
            }
            Err(_) => false,
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

    fn on_chart_right_click(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if let Some((index, x, y)) = self.pick(event.position) {
            let chart = self.chart.clone();
            if let Some(chart) = chart {
                let node = chart.slices[index].node.clone();
                if node.kind != Kind::Other && !node.path.is_empty() {
                    self.menu = Some(Menu { x, y, node });
                    cx.notify();
                }
            }
        }
    }

    fn on_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                if self.confirm.is_some() {
                    self.confirm = None;
                } else if self.menu.is_some() {
                    self.menu = None;
                } else {
                    self.hover = None;
                    self.tooltip = None;
                }
                cx.notify();
            }
            "backspace" | "arrowup" | "arrowleft" => self.go_up(cx),
            _ => {}
        }
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
                    .child(
                        div()
                            .truncate()
                            .text_size(px(10.0))
                            .text_color(theme::text_faint())
                            .child(focus.path.clone()),
                    )
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
                                    crate::sys::reveal(&focus.path);
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

        if let Some(menu) = &self.menu {
            let node = menu.node.clone();
            let has_path = !node.path.is_empty() && node.kind != Kind::Other;
            let delete_label = if node.is_dir() {
                "Delete folder…"
            } else {
                "Delete file…"
            };
            let reveal_node = node.clone();
            let delete_node = node.clone();

            layers.push(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .w_full()
                    .h_full()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            this.menu = None;
                            cx.notify();
                        }),
                    )
                    .into_any_element(),
            );

            layers.push(
                div()
                    .absolute()
                    .left(px(menu.x))
                    .top(px(menu.y))
                    .w(px(232.0))
                    .p(px(5.0))
                    .rounded(px(10.0))
                    .bg(theme::surface_alt())
                    .border_1()
                    .border_color(theme::hairline())
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .px(px(9.0))
                            .py(px(4.0))
                            .text_size(px(9.5))
                            .text_color(theme::text_faint())
                            .truncate()
                            .child(short_name(&node.name, 30)),
                    )
                    .child(menu_item("Reveal in Explorer", false, has_path, cx, move |this, cx| {
                        crate::sys::reveal(&reveal_node.path);
                        this.menu = None;
                        cx.notify();
                    }))
                    .child(div().h(px(1.0)).my(px(3.0)).bg(theme::hairline()))
                    .child(menu_item(delete_label, true, has_path, cx, move |this, cx| {
                        this.ask_delete(delete_node.clone(), cx);
                    }))
                    .into_any_element(),
            );
        }

        if let Some(confirm) = &self.confirm {
            let node = confirm.node.clone();
            let detail = if node.is_dir() {
                format!(
                    "{} · {} files",
                    theme::format_bytes(node.size),
                    theme::format_count(node.files)
                )
            } else {
                theme::format_bytes(node.size)
            };
            layers.push(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(0.))
                    .w_full()
                    .h_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui::rgba(0x05070bcc))
                    .child(
                        div()
                            .w(px(420.0))
                            .p(px(18.0))
                            .rounded(px(14.0))
                            .bg(theme::surface())
                            .border_1()
                            .border_color(theme::hairline())
                            .flex()
                            .flex_col()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .text_size(px(15.0))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("Move to Recycle Bin?"),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme::text())
                                    .child(node.name.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme::text_faint())
                                    .truncate()
                                    .child(node.path.clone()),
                            )
                            .child(
                                div()
                                    .text_size(px(10.5))
                                    .text_color(theme::text_dim())
                                    .child(detail),
                            )
                            .child(
                                div()
                                    .mt(px(6.0))
                                    .flex()
                                    .justify_end()
                                    .gap(px(8.0))
                                    .child(self.action_button("Cancel", cx, |this, cx| {
                                        this.confirm = None;
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .id("confirm-delete")
                                            .px(px(12.0))
                                            .py(px(5.0))
                                            .rounded(px(7.0))
                                            .bg(theme::danger())
                                            .text_size(px(11.5))
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(gpui::rgb(0x1a0b0b))
                                            .cursor_pointer()
                                            .hover(|style| style.opacity(0.88))
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                this.confirm_delete(cx);
                                            }))
                                            .child("Delete"),
                                    ),
                            ),
                    )
                    .into_any_element(),
            );
        }

        if matches!(self.phase, Phase::Scanning) {
            let (files, bytes, path) = &self.progress;
            layers.push(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(TOPBAR_H))
                    .w_full()
                    .h(px(52.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .px(px(14.0))
                            .py(px(7.0))
                            .rounded(px(10.0))
                            .bg(theme::surface_alt())
                            .border_1()
                            .border_color(theme::hairline())
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .text_size(px(11.0))
                            .text_color(theme::text_dim())
                            .child(div().text_color(theme::accent()).child("●"))
                            .child(format!(
                                "{} files · {} · {}",
                                theme::format_count(*files),
                                theme::format_bytes(*bytes),
                                short_name(path, 60)
                            )),
                    )
                    .into_any_element(),
            );
        }

        layers
    }
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

fn menu_item(
    label: &str,
    danger: bool,
    enabled: bool,
    cx: &mut Context<DiskApp>,
    action: impl Fn(&mut DiskApp, &mut Context<DiskApp>) + 'static,
) -> gpui::Stateful<Div> {
    let base = div()
        .id(SharedString::from(format!("menu-{label}")))
        .px(px(9.0))
        .py(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.5))
        .child(label.to_string());

    if !enabled {
        return base.text_color(theme::text_faint()).opacity(0.5);
    }

    let base = base
        .cursor_pointer()
        .text_color(if danger {
            theme::danger()
        } else {
            theme::text()
        })
        .hover(|style| style.bg(theme::hairline()))
        .on_click(cx.listener(move |this, _, _, cx| action(this, cx)));
    base
}

fn ancestors(chart: &ChartModel, index: usize) -> HashSet<usize> {
    let mut set = HashSet::new();
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
                    .child(self.chart_area(viewport_w, viewport_h, cx))
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
}

#[allow(dead_code)]
const _: usize = MAX_RINGS;
#[allow(dead_code)]
const _: f32 = TAU;

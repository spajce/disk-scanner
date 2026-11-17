//! The scan tree as the UI uses it, plus the sunburst layout and hit testing.
//!
//! `disk_scan::to_dto` hands back an owned `NodeDto` tree; we convert it once
//! into `Arc<Node>` so slices can hand cheap clones to paint closures.

use std::f32::consts::{PI, TAU};
use std::sync::Arc;

use disk_scan::NodeDto;

/// At most this many rings are drawn below the focused folder.
pub const MAX_RINGS: usize = 5;
/// Children narrower than this (in radians) are skipped: they would be
/// sub-pixel slivers that only cost tessellation time.
pub const MIN_SPAN: f32 = 0.0016;
/// Hard cap on slices per frame, so a pathological folder cannot stall paint.
pub const MAX_SLICES: usize = 1800;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric {
    Bytes,
    Files,
}

impl Metric {
    pub fn weight(self, node: &Node) -> f64 {
        match self {
            Metric::Bytes => node.size as f64,
            Metric::Files => node.files as f64,
        }
    }

    pub fn unit(self) -> &'static str {
        match self {
            Metric::Bytes => "BYTES",
            Metric::Files => "FILES",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Other,
}

pub struct Node {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub files: u64,
    pub dirs: u64,
    pub kind: Kind,
    pub children: Vec<Arc<Node>>,
}

impl Node {
    pub fn from_dto(dto: &NodeDto) -> Arc<Node> {
        Arc::new(Node {
            name: dto.name.clone(),
            path: dto.path.clone(),
            size: dto.size,
            files: dto.files,
            dirs: dto.dirs,
            kind: match dto.kind {
                "dir" => Kind::Dir,
                "file" => Kind::File,
                _ => Kind::Other,
            },
            children: dto.children.iter().map(Node::from_dto).collect(),
        })
    }

    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }

    /// Depth-first lookup by absolute path.
    pub fn find(node: &Arc<Node>, path: &str) -> Option<Arc<Node>> {
        if node.path.eq_ignore_ascii_case(path) {
            return Some(node.clone());
        }
        for child in &node.children {
            if child.path.eq_ignore_ascii_case(path) {
                return Some(child.clone());
            }
            if child.is_dir() {
                if let Some(found) = Node::find(child, path) {
                    return Some(found);
                }
            }
        }
        None
    }
}

pub struct Slice {
    pub node: Arc<Node>,
    /// 1 for the ring around the hub.
    pub depth: usize,
    pub x0: f32,
    pub x1: f32,
    pub parent: usize,
    pub hue: f32,
}

impl Slice {
    pub fn span(&self) -> f32 {
        self.x1 - self.x0
    }

    pub fn mid(&self) -> f32 {
        (self.x0 + self.x1) * 0.5
    }
}

pub struct ChartModel {
    pub focus: Arc<Node>,
    pub metric: Metric,
    pub slices: Vec<Slice>,
    pub rings: usize,
    /// Weight of the focused folder, i.e. 100% of the ring.
    pub total: f64,
}

pub fn build_chart(focus: Arc<Node>, metric: Metric) -> ChartModel {
    let total = metric.weight(&focus);
    let rings = (tree_depth(&focus) + 1).min(MAX_RINGS);
    let mut model = ChartModel {
        focus,
        metric,
        slices: Vec::new(),
        rings,
        total,
    };
    if total <= 0.0 {
        return model;
    }

    // Seed the first ring, then descend breadth-first.
    let focus = model.focus.clone();
    let mut stack: Vec<(Arc<Node>, usize, f32, f32, usize, f32)> = Vec::new();
    let mut angle = -PI / 2.0;
    for (index, child) in focus.children.iter().enumerate() {
        let weight = metric.weight(child);
        if weight <= 0.0 {
            continue;
        }
        let span = (weight / total) as f32 * TAU;
        if span < MIN_SPAN {
            continue;
        }
        stack.push((
            child.clone(),
            1,
            angle,
            angle + span,
            usize::MAX,
            crate::theme::root_hue(index),
        ));
        angle += span;
    }
    stack.reverse();

    while let Some((node, depth, x0, x1, parent, hue)) = stack.pop() {
        if model.slices.len() >= MAX_SLICES {
            break;
        }
        let index = model.slices.len();
        model.slices.push(Slice {
            node: node.clone(),
            depth,
            x0,
            x1,
            parent,
            hue,
        });

        if depth >= rings || !node.is_dir() || node.children.is_empty() {
            continue;
        }

        let mut inner = x0;
        let mut pending = Vec::new();
        for (child_index, child) in node.children.iter().enumerate() {
            let weight = metric.weight(child);
            if weight <= 0.0 {
                continue;
            }
            let span = (weight / total) as f32 * TAU;
            if span < MIN_SPAN {
                continue;
            }
            pending.push((
                child.clone(),
                depth + 1,
                inner,
                inner + span,
                index,
                crate::theme::child_hue(hue, child_index),
            ));
            inner += span;
        }
        pending.reverse();
        stack.extend(pending);
    }

    // Ring count follows what was actually drawn: an arm with no deep folders
    // would otherwise leave an empty outer band and shrink the whole chart.
    // Depth 1 is the first band, so the count is the deepest drawn depth.
    let depth_max = model.slices.iter().map(|slice| slice.depth).max().unwrap_or(0);
    model.rings = depth_max.min(MAX_RINGS).max(1);

    model
}

fn tree_depth(node: &Arc<Node>) -> usize {
    let mut best = 0;
    for child in &node.children {
        best = best.max(1 + tree_depth(child));
        if best >= MAX_RINGS {
            return MAX_RINGS;
        }
    }
    best
}

/// Pixel geometry of the chart, derived from the drawn square.
#[derive(Clone, Copy, Debug)]
pub struct Geom {
    pub cx: f32,
    pub cy: f32,
    pub inner: f32,
    pub ring_w: f32,
    pub rings: usize,
}

impl Geom {
    pub fn new(side: f32, rings: usize) -> Self {
        let radius = (side * 0.5 - 6.0).max(10.0);
        let inner = radius * 0.34;
        let rings = rings.max(1);
        Geom {
            cx: side * 0.5,
            cy: side * 0.5,
            inner,
            ring_w: (radius - inner) / rings as f32,
            rings,
        }
    }

    pub fn radius(&self) -> f32 {
        self.inner + self.ring_w * self.rings as f32
    }

    pub fn ring_bounds(&self, depth: usize) -> (f32, f32) {
        let r0 = self.inner + self.ring_w * (depth - 1) as f32;
        (r0, r0 + self.ring_w)
    }

    /// Where the cursor sits, in chart-local pixels.
    pub fn angle_at(&self, x: f32, y: f32) -> Option<(f32, usize)> {
        let dx = x - self.cx;
        let dy = y - self.cy;
        let radius = (dx * dx + dy * dy).sqrt();
        if radius < self.inner || radius > self.radius() {
            return None;
        }
        let ring = ((radius - self.inner) / self.ring_w).floor() as usize;
        if ring >= self.rings {
            return None;
        }
        let mut angle = dy.atan2(dx);
        while angle < -PI / 2.0 {
            angle += TAU;
        }
        Some((angle, ring + 1))
    }
}

/// Which slice is under the cursor, if any.
pub fn hit(model: &ChartModel, geom: &Geom, x: f32, y: f32) -> Option<usize> {
    let (angle, depth) = geom.angle_at(x, y)?;
    let mut best: Option<usize> = None;
    for (index, slice) in model.slices.iter().enumerate() {
        if slice.depth != depth {
            continue;
        }
        if angle >= slice.x0 && angle < slice.x1 {
            if best.is_none_or(|current| slice.x0 > model.slices[current].x0) {
                best = Some(index);
            }
        }
    }
    best
}

/// `C:\Users\jcarillo\AppData` -> `AppData`, plus an ancestor chain for crumbs.
pub fn short_name(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let mut out: String = name.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

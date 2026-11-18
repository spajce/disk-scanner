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
/// Height of the name strip a folder keeps at the top of its treemap cell.
pub const TREEMAP_HEADER: f32 = 13.0;

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

/// Which drawing algorithm the chart area uses.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Rings,
    Treemap,
    Heatmap,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Rings => "Rings",
            View::Treemap => "Treemap",
            View::Heatmap => "Heatmap",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            View::Rings => "rings",
            View::Treemap => "treemap",
            View::Heatmap => "heatmap",
        }
    }

    pub fn parse(value: &str) -> Option<View> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rings" | "sunburst" => Some(View::Rings),
            "treemap" | "tree" => Some(View::Treemap),
            "heatmap" | "heat" => Some(View::Heatmap),
            _ => None,
        }
    }
}

/// A rectangle in chart-local logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Tile {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Tile {
    pub fn area(&self) -> f32 {
        self.w * self.h
    }

    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    fn inset(&self, by: f32) -> Tile {
        Tile {
            x: self.x + by,
            y: self.y + by,
            w: (self.w - by * 2.0).max(0.0),
            h: (self.h - by * 2.0).max(0.0),
        }
    }
}

/// Weight of the focused folder, i.e. 100% of the ring.
pub struct ChartModel {
    pub focus: Arc<Node>,
    pub metric: Metric,
    pub slices: Vec<Slice>,
    pub rings: usize,
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

/// One rectangle per chart slice (same indices), laid out as a nested,
/// squarified treemap: each folder's children fill the folder's own rectangle.
/// Deeper levels get a 1 px inset so borders stay visible.
pub fn treemap(model: &ChartModel, width: f32, height: f32) -> Vec<Tile> {
    let mut tiles = vec![Tile::default(); model.slices.len()];
    if width <= 1.0 || height <= 1.0 || model.slices.is_empty() {
        return tiles;
    }

    // children[parent + 1] = slice indices under that parent (0 = the focus).
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); model.slices.len() + 1];
    for (index, slice) in model.slices.iter().enumerate() {
        let slot = if slice.parent == usize::MAX {
            0
        } else {
            slice.parent + 1
        };
        children[slot].push(index);
    }

    let root = Tile {
        x: 0.0,
        y: 0.0,
        w: width,
        h: height,
    };

    // Iterative depth-first so a pathological tree cannot blow the stack.
    let mut stack: Vec<(Tile, usize)> = vec![(root, 0)];
    while let Some((rect, parent)) = stack.pop() {
        let kids = &children[parent];
        if kids.is_empty() {
            continue;
        }
        let mut weights: Vec<f32> = kids.iter().map(|&i| model.slices[i].span()).collect();
        // Squarify wants the biggest items first.
        let mut order: Vec<usize> = (0..kids.len()).collect();
        order.sort_by(|&a, &b| weights[b].total_cmp(&weights[a]));
        let sorted: Vec<f32> = order.iter().map(|&i| weights[i]).collect();
        weights = sorted;

        let mut placed = vec![Tile::default(); kids.len()];
        squarify(&weights, rect, &mut placed);
        for (slot, &kid) in order.iter().enumerate() {
            tiles[kids[kid]] = placed[slot];
        }

        // Recurse into folders that have a rectangle worth subdividing. The
        // top strip of the folder's own rectangle is kept for its name, so
        // nested labels never land on top of each other.
        for &kid in kids.iter() {
            let tile = tiles[kid];
            if !children[kid + 1].is_empty() && tile.w > 40.0 && tile.h > 40.0 {
                let inner = if tile.h > TREEMAP_HEADER + 22.0 {
                    Tile {
                        x: tile.x + 1.0,
                        y: tile.y + TREEMAP_HEADER,
                        w: (tile.w - 2.0).max(0.0),
                        h: (tile.h - TREEMAP_HEADER - 1.0).max(0.0),
                    }
                } else {
                    tile.inset(1.0)
                };
                stack.push((inner, kid + 1));
            }
        }
    }

    tiles
}

/// Squarified treemap placement of `weights` (descending) inside `rect`.
fn squarify(weights: &[f32], rect: Tile, out: &mut [Tile]) {
    let total: f32 = weights.iter().sum();
    if total <= 0.0 {
        return;
    }
    let scale = rect.area() / total;
    let areas: Vec<f32> = weights.iter().map(|w| (w * scale).max(0.0)).collect();

    let mut x = rect.x;
    let mut y = rect.y;
    let mut w = rect.w;
    let mut h = rect.h;
    let mut cursor = 0;

    while cursor < areas.len() {
        let side = w.min(h);
        if side < 0.5 || w <= 0.0 || h <= 0.0 {
            break;
        }

        // Grow the row while that keeps the worst aspect ratio improving.
        let mut end = cursor;
        let mut row_sum = 0.0f32;
        let mut row_min = f32::MAX;
        let mut row_max = 0.0f32;
        let mut worst = f32::MAX;
        while end < areas.len() {
            let next_sum = row_sum + areas[end];
            let next_min = row_min.min(areas[end]);
            let next_max = row_max.max(areas[end]);
            let ratio = worst_ratio(next_sum, next_min, next_max, side);
            if ratio > worst {
                break;
            }
            row_sum = next_sum;
            row_min = next_min;
            row_max = next_max;
            worst = ratio;
            end += 1;
        }
        if end == cursor {
            row_sum = areas[cursor];
            end = cursor + 1;
        }

        let thickness = (row_sum / side).min(if w >= h { w } else { h });
        let mut offset = 0.0;
        for index in cursor..end {
            let length = if row_sum > 0.0 {
                areas[index] / row_sum * side
            } else {
                0.0
            };
            out[index] = if w >= h {
                Tile {
                    x,
                    y: y + offset,
                    w: thickness,
                    h: length,
                }
            } else {
                Tile {
                    x: x + offset,
                    y,
                    w: length,
                    h: thickness,
                }
            };
            offset += length;
        }

        if w >= h {
            x += thickness;
            w -= thickness;
        } else {
            y += thickness;
            h -= thickness;
        }
        cursor = end;
    }
}

fn worst_ratio(sum: f32, min: f32, max: f32, side: f32) -> f32 {
    if sum <= 0.0 || min <= 0.0 {
        return f32::MAX;
    }
    let side2 = side * side;
    let sum2 = sum * sum;
    (side2 * max / sum2).max(sum2 / (side2 * min))
}

/// Equal-sized cells for every visible item, in weight order. The caller turns
/// each item's weight into a colour on a log scale, which is what makes a pile
/// of tiny files visible instead of a thin sliver.
pub fn heatmap(model: &ChartModel, width: f32, height: f32) -> Vec<Tile> {
    let count = model.slices.len();
    let mut tiles = vec![Tile::default(); count];
    if count == 0 || width <= 1.0 || height <= 1.0 {
        return tiles;
    }

    let aspect = (width / height).max(0.2);
    let cols = ((count as f32 * aspect).sqrt().ceil() as usize).clamp(1, count);
    let rows = count.div_ceil(cols);
    let cell_w = width / cols as f32;
    let cell_h = height / rows as f32;

    // Biggest first, so the hot cells sit at the top left where the eye lands.
    let mut order: Vec<usize> = (0..count).collect();
    order.sort_by(|&a, &b| {
        model.slices[b]
            .span()
            .total_cmp(&model.slices[a].span())
            .then_with(|| model.slices[a].depth.cmp(&model.slices[b].depth))
    });

    for (slot, &index) in order.iter().enumerate() {
        let row = slot / cols;
        let col = slot % cols;
        // A hair of gutter so neighbouring cells stay distinguishable.
        let gap = if cell_w.min(cell_h) > 8.0 { 1.0 } else { 0.0 };
        tiles[index] = Tile {
            x: col as f32 * cell_w + gap * 0.5,
            y: row as f32 * cell_h + gap * 0.5,
            w: (cell_w - gap).max(1.0),
            h: (cell_h - gap).max(1.0),
        };
    }

    tiles
}

/// Log-scaled position of a weight inside `[min, max]`, in `0.0..=1.0`.
pub fn log_rank(weight: f64, min: f64, max: f64) -> f32 {
    if weight <= 0.0 {
        return 0.0;
    }
    let lo = (min.max(1.0)).ln();
    let hi = max.max(min).max(1.0).ln();
    if hi <= lo {
        return 0.5;
    }
    (((weight.ln() - lo) / (hi - lo)) as f32).clamp(0.0, 1.0)
}


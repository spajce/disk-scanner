//! Colours, the sunburst palette, and number formatting.

use gpui::{hsla, Hsla, Rgba};

/// App background — near-black with a blue cast.
pub fn bg() -> Rgba {
    gpui::rgb(0x0a0d13)
}
pub fn surface() -> Rgba {
    gpui::rgb(0x11151d)
}
pub fn surface_alt() -> Rgba {
    gpui::rgb(0x161b25)
}
pub fn hairline() -> Rgba {
    gpui::rgb(0x232a36)
}
pub fn text() -> Rgba {
    gpui::rgb(0xe9edf5)
}
pub fn text_dim() -> Rgba {
    gpui::rgb(0x98a3b6)
}
pub fn text_faint() -> Rgba {
    gpui::rgb(0x6a7486)
}
pub fn accent() -> Rgba {
    gpui::rgb(0x6aa8ff)
}
pub fn accent_soft() -> Rgba {
    gpui::rgb(0x1d2c45)
}
pub fn danger() -> Rgba {
    gpui::rgb(0xff6b6b)
}
pub fn track() -> Rgba {
    gpui::rgb(0x1c2330)
}

/// Root ring hues, in degrees. Adjacent entries are far apart on the wheel so
/// neighbouring top-level folders never read as one continuous mass.
const PALETTE: [f32; 14] = [
    206.0, 24.0, 118.0, 320.0, 168.0, 42.0, 262.0, 92.0, 8.0, 288.0, 146.0, 66.0, 342.0, 188.0,
];

/// Offsets applied to a child's hue relative to its parent, so neighbouring
/// rings stay related without ever repeating the exact same colour.
const HUE_STEPS: [f32; 7] = [-26.0, 0.0, 26.0, -13.0, 13.0, -39.0, 39.0];

pub fn root_hue(index: usize) -> f32 {
    PALETTE[index % PALETTE.len()]
}

pub fn child_hue(parent: f32, index: usize) -> f32 {
    let step = HUE_STEPS[index % HUE_STEPS.len()];
    (parent + step).rem_euclid(360.0)
}

/// A slice colour: hue carries identity, saturation/lightness fall off with
/// depth so deeper rings read as a quieter shade of the same family.
pub fn slice_color(hue: f32, depth: usize) -> Hsla {
    let sat = (44.0 - (depth as f32 - 1.0) * 2.5).max(28.0) / 100.0;
    let light = (58.0 - (depth as f32 - 1.0) * 4.5).max(34.0) / 100.0;
    hsla(hue / 360.0, sat, light, 1.0)
}

/// Aggregated "(n more)" slices are deliberately colourless so they read as
/// "the rest", not as another folder.
pub fn rest_color(depth: usize) -> Hsla {
    let light = (46.0 - (depth as f32 - 1.0) * 3.0).max(30.0) / 100.0;
    hsla(220.0 / 360.0, 0.08, light, 1.0)
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[0])
    } else if value >= 100.0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else if value >= 10.0 {
        format!("{:.1} {}", value, UNITS[unit])
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}

pub fn format_count(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 10_000 {
        format!("{:.0}K", count as f64 / 1_000.0)
    } else if count >= 1_000 {
        format!("{:.1}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

pub fn format_percent(fraction: f64) -> String {
    let pct = fraction * 100.0;
    if pct >= 10.0 {
        format!("{pct:.0}%")
    } else if pct >= 1.0 {
        format!("{pct:.1}%")
    } else if pct > 0.0 {
        "<1%".to_string()
    } else {
        "0%".to_string()
    }
}

pub fn format_seconds(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.0} s")
    } else {
        let minutes = (seconds / 60.0).floor();
        let rest = seconds - minutes * 60.0;
        format!("{minutes:.0} m {rest:.0} s")
    }
}

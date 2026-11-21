//! Colours, the sunburst palette, and number formatting.
//!
//! The palette is global state so every render helper can keep calling
//! `theme::bg()`-style accessors while the app switches between its light and
//! dark skins at runtime.

use std::sync::{LazyLock, RwLock};

use gpui::{hsla, Hsla, Rgba};

/// Which skin the app should wear. `System` follows the desktop's own light or
/// dark appearance setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    System,
    Dark,
    Light,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::System => "Auto",
            Mode::Dark => "Dark",
            Mode::Light => "Light",
        }
    }

    pub fn next(self) -> Mode {
        match self {
            Mode::System => Mode::Dark,
            Mode::Dark => Mode::Light,
            Mode::Light => Mode::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::System => "system",
            Mode::Dark => "dark",
            Mode::Light => "light",
        }
    }

    pub fn parse(text: &str) -> Option<Mode> {
        match text.trim().to_ascii_lowercase().as_str() {
            "system" | "auto" => Some(Mode::System),
            "dark" => Some(Mode::Dark),
            "light" => Some(Mode::Light),
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Palette {
    pub bg: Rgba,
    pub surface: Rgba,
    pub surface_alt: Rgba,
    pub hairline: Rgba,
    pub text: Rgba,
    pub text_dim: Rgba,
    pub text_faint: Rgba,
    pub accent: Rgba,
    pub accent_soft: Rgba,
    pub danger: Rgba,
    pub track: Rgba,
    pub scrim: Rgba,
    pub light: bool,
}

impl Palette {
    const fn dark() -> Palette {
        Palette {
            bg: Rgba { r: 0.0392, g: 0.0510, b: 0.0745, a: 1.0 },
            surface: Rgba { r: 0.0667, g: 0.0824, b: 0.1137, a: 1.0 },
            surface_alt: Rgba { r: 0.0863, g: 0.1059, b: 0.1451, a: 1.0 },
            hairline: Rgba { r: 0.1373, g: 0.1647, b: 0.2118, a: 1.0 },
            text: Rgba { r: 0.9137, g: 0.9294, b: 0.9608, a: 1.0 },
            text_dim: Rgba { r: 0.5961, g: 0.6392, b: 0.7137, a: 1.0 },
            text_faint: Rgba { r: 0.4157, g: 0.4549, b: 0.5255, a: 1.0 },
            accent: Rgba { r: 0.4157, g: 0.6588, b: 1.0, a: 1.0 },
            accent_soft: Rgba { r: 0.1137, g: 0.1725, b: 0.2706, a: 1.0 },
            danger: Rgba { r: 1.0, g: 0.4196, b: 0.4196, a: 1.0 },
            track: Rgba { r: 0.1098, g: 0.1373, b: 0.1882, a: 1.0 },
            scrim: Rgba { r: 0.0, g: 0.0, b: 0.0, a: 0.55 },
            light: false,
        }
    }

    const fn light() -> Palette {
        Palette {
            bg: Rgba { r: 0.9647, g: 0.9686, b: 0.9843, a: 1.0 },
            surface: Rgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            surface_alt: Rgba { r: 0.9333, g: 0.9451, b: 0.9686, a: 1.0 },
            hairline: Rgba { r: 0.8471, g: 0.8706, b: 0.9137, a: 1.0 },
            text: Rgba { r: 0.0706, g: 0.0941, b: 0.1255, a: 1.0 },
            text_dim: Rgba { r: 0.3176, g: 0.3765, b: 0.4784, a: 1.0 },
            text_faint: Rgba { r: 0.4863, g: 0.5412, b: 0.6431, a: 1.0 },
            accent: Rgba { r: 0.1843, g: 0.4353, b: 0.9294, a: 1.0 },
            accent_soft: Rgba { r: 0.8588, g: 0.9059, b: 1.0, a: 1.0 },
            danger: Rgba { r: 0.8510, g: 0.1686, b: 0.2706, a: 1.0 },
            track: Rgba { r: 0.8863, g: 0.9059, b: 0.9451, a: 1.0 },
            scrim: Rgba { r: 0.0706, g: 0.0941, b: 0.1255, a: 0.35 },
            light: true,
        }
    }
}

static CURRENT: LazyLock<RwLock<Palette>> = LazyLock::new(|| RwLock::new(Palette::dark()));

fn palette() -> Palette {
    CURRENT
        .read()
        .map(|guard| *guard)
        .unwrap_or_else(|poisoned| *poisoned.into_inner())
}

/// Install the skin for `mode`; `system_dark` is the OS preference, only used
/// when the mode is `System`.
pub fn apply(mode: Mode, system_dark: bool) {
    let next = match mode {
        Mode::System if system_dark => Palette::dark(),
        Mode::System => Palette::light(),
        Mode::Dark => Palette::dark(),
        Mode::Light => Palette::light(),
    };
    if let Ok(mut guard) = CURRENT.write() {
        *guard = next;
    }
}

pub fn is_light() -> bool {
    palette().light
}

pub fn bg() -> Rgba {
    palette().bg
}
pub fn surface() -> Rgba {
    palette().surface
}
pub fn surface_alt() -> Rgba {
    palette().surface_alt
}
pub fn hairline() -> Rgba {
    palette().hairline
}
pub fn text() -> Rgba {
    palette().text
}
pub fn text_dim() -> Rgba {
    palette().text_dim
}
pub fn text_faint() -> Rgba {
    palette().text_faint
}
pub fn accent() -> Rgba {
    palette().accent
}
pub fn accent_soft() -> Rgba {
    palette().accent_soft
}
pub fn danger() -> Rgba {
    palette().danger
}
pub fn track() -> Rgba {
    palette().track
}
pub fn scrim() -> Rgba {
    palette().scrim
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
/// depth so deeper rings read as a quieter shade of the same family. On the
/// light skin the arcs need to be deeper and a touch more saturated, otherwise
/// the pastel shades vanish into the white card behind them.
pub fn slice_color(hue: f32, depth: usize) -> Hsla {
    let step = depth.saturating_sub(1) as f32;
    if is_light() {
        let sat = (62.0 - step * 2.0).max(42.0) / 100.0;
        let light = (54.0 - step * 3.6).max(30.0) / 100.0;
        hsla(hue / 360.0, sat, light, 1.0)
    } else {
        let sat = (44.0 - step * 2.5).max(28.0) / 100.0;
        let light = (58.0 - step * 4.5).max(34.0) / 100.0;
        hsla(hue / 360.0, sat, light, 1.0)
    }
}

/// Aggregated "(n more)" slices are deliberately colourless so they read as
/// "the rest", not as another folder.
pub fn rest_color(depth: usize) -> Hsla {
    let step = depth.saturating_sub(1) as f32;
    if is_light() {
        let light = (78.0 - step * 3.0).max(62.0) / 100.0;
        hsla(220.0 / 360.0, 0.06, light, 1.0)
    } else {
        let light = (46.0 - step * 3.0).max(30.0) / 100.0;
        hsla(220.0 / 360.0, 0.08, light, 1.0)
    }
}

/// Heatmap colour for a log-scaled rank in `0.0..=1.0`: cold blue for the
/// smallest cells, through teal and green, to red for the biggest. Equal-sized
/// cells plus this ramp is what makes "a thousand small files" readable.
pub fn heat_color(rank: f32) -> Hsla {
    let rank = rank.clamp(0.0, 1.0);
    let hue = 205.0 * (1.0 - rank);
    let (sat, light) = if is_light() {
        (0.66, 0.44 + rank * 0.08)
    } else {
        (0.58, 0.34 + rank * 0.24)
    };
    hsla(hue / 360.0, sat, light, 1.0)
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

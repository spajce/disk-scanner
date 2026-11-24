// The app is a GUI, not a console tool: without this attribute Windows opens a
// console window next to the sunburst whenever the .exe is launched. Other
// platforms ignore it.
#![windows_subsystem = "windows"]

//! Native disk-usage explorer: a zoomable sunburst, drawn with GPUI.

mod app;
mod model;
mod sys;
mod theme;
mod update;

use gpui::Application;

fn main() {
    Application::new().run(|cx| {
        app::open(cx);
    });
}

//! Native Windows disk-usage explorer: a zoomable sunburst, drawn with GPUI.

mod app;
mod model;
mod sys;
mod theme;

use gpui::Application;

fn main() {
    Application::new().run(|cx| {
        app::open(cx);
    });
}

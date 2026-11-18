//! Embeds the window/executable icon so the taskbar, alt-tab and Explorer all
//! show the sunburst mark instead of the generic Rust binary icon.

fn main() {
    println!("cargo:rerun-if-changed=assets/app.rc");
    println!("cargo:rerun-if-changed=assets/icon.ico");

    #[cfg(windows)]
    {
        let _ = embed_resource::compile("assets/app.rc", embed_resource::NONE);
    }
}

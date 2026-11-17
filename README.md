# Disk Scanner

A desktop disk-usage explorer that renders a whole volume as a zoomable **sunburst** —
every folder level at once, sized by bytes or by file count, in the spirit of the classic
Scanner (Steffen Gerlach, 2012) but rebuilt as a native Rust app.

![Disk Scanner](docs/screenshot.png)

## What it does

- **Scans a volume or folder in parallel** with a `rayon`-backed walker; one pass, no
  index file, cancellable at any time.
- **Renders a multilevel sunburst** on the GPU through GPUI (Zed's UI framework): the hub
  shows the current folder, each ring is one directory level deeper. Click a slice to zoom
  in; hover to trace a slice's ancestry; the hub, labels and the breakdown panel follow.
- **Weights rings by size or by file count** — the same tree, re-proportioned.
- **Breakdown panel** lists the biggest direct children of the focused folder, with a
  visual share meter, and syncs hover highlighting with the chart.
- **Right-click actions** on a slice or a breakdown row: *Reveal in Explorer*, and
  *Delete folder* / *Delete file* (moves it to the Recycle Bin after a confirmation).
- **Live progress** while scanning (files, bytes, current path) plus per-volume free space
  in the top bar.
- **Volume switching never blocks**: a volume already walked in this session is painted from
  memory the instant you click its chip, while the fresh scan runs behind the chart with a
  progress pill — the window never blanks out or forgets where you were.
- **Auto / Dark / Light theme**: the `◐` toggle follows the Windows app theme by default and
  remembers your choice in `%APPDATA%\disk-scanner\theme.txt`.
- **Keyboard**: `Backspace` / `↑` / `←` zoom out, `Esc` closes the menu, dialog or tooltip
  (or stops a running scan).

No webview: the window, the chart and the panels are drawn by GPUI directly.

## Stack

| Layer | Choice |
| --- | --- |
| UI | [GPUI](https://crates.io/crates/gpui) 0.2 — native Rust rendering, no HTML/CSS |
| Scanner | Rust — `windows-sys` for volume info and the shell, `rayon` for the parallel walk |
| Chart | hand-rolled arc geometry (`PathBuilder` → `Window::paint_path`), no plotting crate |

## Layout

```
apps/desktop-gpui/
  src/main.rs    entry point: opens the 1320x880 window
  src/app.rs     the view: top bar, chart area, breakdown panel, status bar, overlays,
                 scan job plumbing, zoom state, context menu + delete flow
  src/model.rs   tree model, chart layout (rings/slices), hit testing, geometry
  src/theme.rs   colours, palette, number formatting
  src/sys.rs     Explorer reveal + Recycle Bin (SHFileOperationW)
crates/disk-scan/
  src/scanner.rs parallel directory walk, hit counters, tree compaction
  src/drives.rs  Win32 volume enumeration (GetLogicalDrives, GetDiskFreeSpaceExW, …)
scripts/
  rust-env.ps1   runs cargo inside a working Visual Studio developer environment
```

### How the tree is made

The scanner accumulates sizes bottom-up in one parallel pass. Before it reaches the UI,
the tree is compacted so a 2-million-file volume still renders smoothly:

- children sorted by size; the long tail is folded into one `(N more)` slice
  (`REL_FLOOR`, `ABS_FLOOR` thresholds);
- loose files below `FILE_FLOOR` (and the small tail of a directory) become one
  `smaller files` slice, so a folder's arcs always add up to the number in the hub;
- single-child chains are collapsed into `a/b/c` slices;
- the total node count is capped (`NODE_BUDGET`) and depth is bounded.

The UI never walks a raw directory tree — it only lays out what the scanner decided is
worth showing, and caps itself at `MAX_RINGS` rings and `MAX_SLICES` arcs per view.

## Building

Requires Rust 1.85+, the MSVC toolchain and the Windows SDK.

```bash
cargo build --release          # target/release/disk-scanner.exe
cargo run                      # debug build, scans the last folder you opened
cargo test                     # scanner tests + Recycle Bin integration test
```

On a machine with several Visual Studio installs, run cargo inside a correctly configured
environment instead:

```powershell
scripts\rust-env.ps1 cargo build --release
```

The app remembers the last scanned root in `%APPDATA%\disk-scanner\last-root.txt` and
rescans it on start (falling back to the first fixed drive). Scans are cached per volume for the
session, so switching between drives repaints instantly and refreshes behind the chart.

### Running on a machine with several Visual Studio installs

`rustc` and the `cc` crate both pick the *newest* Visual Studio install. If that install is
incomplete (a pre-release toolset without headers or `lib\x64`), a plain `cargo build`
fails with `LNK1104: cannot open file 'msvcrt.lib'`. `scripts/rust-env.ps1` finds the first
install whose MSVC toolset actually contains `include\excpt.h` + `lib\x64\msvcrt.lib`,
imports the environment from its `vcvars64.bat`, and then invokes cargo.

## Notes

- Symlinks and every other reparse point (junctions, mount points, dedup stubs) are
  skipped, so `Users\All Users -> ProgramData` cannot double-count a volume and junction
  loops cannot trap the walk.
- Paths longer than `MAX_PATH` are handled through the `\\?\` verbatim form internally.
- Files that vanish mid-scan (locked, deleted, permission denied) are counted as skipped
  errors instead of aborting the scan.
- Deletes go through `SHFileOperationW` with `FOF_ALLOWUNDO`, so they land in the Recycle
  Bin; drive roots are refused.
- The first prototype of this app was a Tauri + TypeScript build; it lives in git history
  at commit `bbb8283`. The native GPUI app replaced it.

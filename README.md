# Disk Scanner

A desktop disk-usage explorer that renders a whole volume as a zoomable **sunburst** —
every folder level at once, sized by bytes or by file count, in the spirit of the classic
Scanner (Steffen Gerlach, 2012) but rebuilt with a Rust scanner and a hand-rolled SVG
chart.

![Disk Scanner](docs/screenshot.png)

## What it does

- **Scans a volume or folder in parallel** with a `rayon`-backed walker; one pass, no
  index file, cancellable at any time.
- **Renders a multilevel sunburst**: the hub shows the current folder, each ring is one
  directory level deeper. Click a slice to zoom in; the hub, rings, breadcrumbs and the
  breakdown panel follow.
- **Weights rings by size or by file count** — the same tree, re-proportioned.
- **Breakdown panel** lists the biggest direct children of the focused folder, with a
  visual share meter, and syncs hover highlighting with the chart.
- **Live progress** while scanning (files, bytes, current path) plus per-volume free space
  in the top bar.
- **Reveal in Explorer**, breadcrumb navigation, and keyboard zoom-out (`Backspace` / `↑`).

## Stack

| Layer | Choice |
| --- | --- |
| Shell | [Tauri 2](https://tauri.app) (WebView2 on Windows) |
| Backend | Rust — `windows-sys` for volume info, `rayon` for the parallel walk, `serde` for the DTO boundary |
| Frontend | TypeScript + Vite, no framework, hand-written SVG chart (no d3) |

## Layout

```
src-tauri/
  src/scanner.rs   parallel directory walk, hit counters, tree compaction
  src/drives.rs    Win32 volume enumeration (GetLogicalDrives, GetDiskFreeSpaceExW, …)
  src/lib.rs       Tauri commands: startup, list_drives, scan, cancel_scan, reveal
  tauri.conf.json  window, bundling, dev/build hooks
src/
  chart.ts         sunburst layout + keyed angle animation + hit testing
  main.ts          app state, zoom stack, breadcrumbs, panels, progress overlay
  demo.ts          deterministic fixture used by the browser preview
  format.ts        byte / count / percent / duration formatting
scripts/
  rust-env.ps1     runs cargo or the Tauri CLI inside a VS developer environment
  make_icon.py     regenerates the sunburst app icon set
```

### How the tree is made

The scanner accumulates sizes bottom-up in one parallel pass. Before it crosses to the
UI, the tree is compacted so a 2-million-file volume still renders smoothly:

- children sorted by size; the long tail is folded into one `(N more)` slice
  (`REL_FLOOR`, `ABS_FLOOR` thresholds);
- loose files below `FILE_FLOOR` (and the small tail of a directory) become one
  `smaller files` slice, so a folder's arcs always add up to the number in the hub;
- single-child chains are collapsed into `a/b/c` slices;
- the total node count is capped (`NODE_BUDGET`), and depth is bounded.

The frontend never walks a raw directory tree — it only lays out what the backend
decided is worth showing.

## Development

```bash
pnpm install
pnpm dev            # browser preview with demo data (no Rust needed)
pnpm tauri dev      # real app against a real volume
pnpm build          # typecheck + production frontend bundle
pnpm test:rust      # scanner unit tests (cargo test via the VS environment)
pnpm test           # typecheck + rust tests
pnpm tauri build    # release app + NSIS installer in src-tauri/target/release/bundle
```

### Running on a machine with several Visual Studio installs

`rustc` and the `cc` crate both pick the *newest* Visual Studio install. If that install
is incomplete (a pre-release toolset without headers or `lib\x64`), a plain `cargo build`
fails with `LNK1104: cannot open file 'msvcrt.lib'`. `scripts/rust-env.ps1` finds the
first install whose MSVC toolset actually contains `include\excpt.h` + `lib\x64\msvcrt.lib`,
imports the environment from its `vcvars64.bat`, and then invokes cargo. Both `pnpm tauri`
and `pnpm cargo` go through it, so on a healthy machine it behaves exactly like a plain
`cargo` call.

## Notes

- Symlinks and every other reparse point (junctions, mount points, dedup stubs) are
  skipped, so `Users\All Users -> ProgramData` cannot double-count a volume and
  junction loops cannot trap the walk.
- Paths longer than `MAX_PATH` are handled through the `\\?\` verbatim form internally.
- Files that vanish mid-scan (locked, deleted, permission denied) are counted as skipped
  errors instead of aborting the scan.

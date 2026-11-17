//! Tauri commands: volume discovery, scanning, cancellation, reveal-in-explorer.

mod drives;
mod scanner;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use scanner::{NodeDto, ScanCounters};

#[derive(Default)]
struct ScanState {
    cancel: Arc<AtomicBool>,
    running: AtomicBool,
}

impl ScanState {
    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Progress {
    files: u64,
    dirs: u64,
    bytes: u64,
    errors: u64,
    current: String,
    elapsed_ms: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanResult {
    root: NodeDto,
    total_size: u64,
    total_files: u64,
    total_dirs: u64,
    errors: u64,
    elapsed_ms: u64,
    cancelled: bool,
    /// Nodes in the payload, useful for diagnostics in the UI.
    node_count: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Startup {
    drives: Vec<drives::DriveInfo>,
    initial_root: String,
    last_root: Option<String>,
}

fn config_file(app: &AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join("last-root.txt"))
}

fn read_last_root(app: &AppHandle) -> Option<String> {
    let path = config_file(app)?;
    let text = std::fs::read_to_string(path).ok()?;
    let text = text.trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

fn write_last_root(app: &AppHandle, root: &str) {
    if let Some(path) = config_file(app) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, root);
    }
}

/// A root requested on the command line, via env var, or the system drive.
fn default_root() -> String {
    if let Some(arg) = std::env::args().nth(1) {
        let path = PathBuf::from(&arg);
        if path.is_dir() {
            return path.to_string_lossy().into_owned();
        }
    }
    if let Ok(from_env) = std::env::var("DISK_SCANNER_ROOT") {
        if PathBuf::from(&from_env).is_dir() {
            return from_env;
        }
    }
    "C:\\".to_string()
}

#[tauri::command]
fn startup(app: AppHandle) -> Startup {
    let last_root = read_last_root(&app).filter(|root| PathBuf::from(root).is_dir());
    let initial_root = last_root.clone().unwrap_or_else(default_root);
    Startup {
        drives: drives::list(),
        initial_root,
        last_root,
    }
}

#[tauri::command]
fn list_drives() -> Vec<drives::DriveInfo> {
    drives::list()
}

#[tauri::command]
async fn scan(
    root: String,
    app: AppHandle,
    state: State<'_, ScanState>,
) -> Result<ScanResult, String> {
    if state.running() {
        return Err("A scan is already running".into());
    }
    let path = PathBuf::from(&root);
    if !path.is_dir() {
        return Err(format!("{} is not an existing folder", root));
    }

    state.running.store(true, Ordering::SeqCst);
    state.cancel.store(false, Ordering::SeqCst);
    let cancel = state.cancel.clone();
    let counters = Arc::new(ScanCounters::new());

    // Progress pump: a plain thread reading the atomics, so the walker never
    // pays for IPC on the hot path.
    let stop = Arc::new(AtomicBool::new(false));
    let pump = {
        let app = app.clone();
        let counters = counters.clone();
        let cancel = cancel.clone();
        let stop = stop.clone();
        let started = Instant::now();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(160));
                let (files, dirs, bytes, errors, current) = counters.snapshot();
                let _ = app.emit(
                    "scan-progress",
                    Progress {
                        files,
                        dirs,
                        bytes,
                        errors,
                        current,
                        elapsed_ms: started.elapsed().as_millis() as u64,
                    },
                );
            }
        })
    };

    let started = Instant::now();
    let walk_counters = counters.clone();
    let walk_cancel = cancel.clone();
    let walk_path = path.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        scanner::scan(&walk_path, &walk_cancel, &walk_counters)
    })
    .await;

    stop.store(true, Ordering::Relaxed);
    let _ = pump.join();
    state.running.store(false, Ordering::SeqCst);

    let raw = match outcome {
        Ok(Ok(raw)) => raw,
        Ok(Err(err)) => return Err(err),
        Err(err) => return Err(format!("scan task failed: {err}")),
    };

    let elapsed_ms = started.elapsed().as_millis() as u64;
    let total_size = raw.size;
    let total_files = raw.files;
    let total_dirs = raw.dirs;
    let (_, _, _, errors, _) = counters.snapshot();
    let cancelled = cancel.load(Ordering::SeqCst);

    let dto = scanner::to_dto(&raw, total_size.max(1));
    let node_count = count_nodes(&dto);
    write_last_root(&app, &root);

    Ok(ScanResult {
        root: dto,
        total_size,
        total_files,
        total_dirs,
        errors,
        elapsed_ms,
        cancelled,
        node_count,
    })
}

fn count_nodes(node: &NodeDto) -> usize {
    1 + node.children.iter().map(count_nodes).sum::<usize>()
}

#[tauri::command]
fn cancel_scan(state: State<'_, ScanState>) {
    state.cancel.store(true, Ordering::SeqCst);
}

#[tauri::command]
fn reveal(path: String) -> Result<(), String> {
    let target = PathBuf::from(&path);
    if !target.exists() {
        return Err(format!("{path} no longer exists"));
    }
    if target.is_dir() {
        std::process::Command::new("explorer.exe")
            .arg(&target)
            .spawn()
            .map_err(|err| err.to_string())?;
    } else {
        std::process::Command::new("explorer.exe")
            .arg(format!("/select,{}", target.display()))
            .spawn()
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(ScanState::default())
        .invoke_handler(tauri::generate_handler![
            startup,
            list_drives,
            scan,
            cancel_scan,
            reveal
        ])
        .setup(|app| {
            // The configured window is a comfortable size on a big display, but
            // it is expressed in logical pixels: on a 125 % scaled laptop it can
            // end up wider than the screen. Shrink it to the monitor work area.
            if let Some(window) = app.get_webview_window("main") {
                if let Ok(Some(monitor)) = window.current_monitor() {
                    let scale = monitor.scale_factor();
                    let area = monitor.size().to_logical::<f64>(scale);
                    let width = 1320.0_f64.min(area.width - 60.0).max(980.0);
                    let height = 880.0_f64.min(area.height - 90.0).max(640.0);
                    let _ = window.set_size(tauri::LogicalSize::new(width, height));
                    let _ = window.center();
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the Disk Scanner window");
}

//! Updates from this project's GitHub releases.
//!
//! The window asks three things of this module: what the newest release is, a
//! copy of its archive on disk, and a way to put that archive's binary in place
//! of the running one. The HTTP and the unpacking live here so the app code
//! stays about the window.
//!
//! The swap is done by a script that runs after this process exits: Windows
//! holds an open executable against writes, and on every platform the cleanest
//! moment to replace the binary is when nothing is running from it.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The version this build reports, straight from `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const API_LATEST: &str = "https://api.github.com/repos/spajce/disk-scanner/releases/latest";

/// A published release, reduced to what the window shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    /// `0.1.5`, without the tag's `v`.
    pub version: String,
    /// The tag as published: `v0.1.5`.
    pub tag: String,
    /// The release page, opened when the archive for this platform is missing.
    pub page: String,
    /// The archive that suits this platform, when the release has one.
    pub asset: Option<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    /// Size in bytes, as GitHub reports it; the download's own header wins.
    pub size: u64,
}

/// Suffix of the release archive built for the platform this binary runs on.
pub fn asset_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => Some("windows-x64.zip"),
        ("macos", "aarch64") => Some("macos-arm64.tar.gz"),
        ("linux", "x86_64") => Some("linux-x64.tar.gz"),
        _ => None,
    }
}

/// Ask GitHub for the newest release. `Err` carries a sentence a status bar can
/// show as is.
pub fn check() -> Result<Release, String> {
    let body = get_text(API_LATEST, true)?;
    parse_release(&body)
}

/// Read the `/releases/latest` payload down to a [`Release`].
fn parse_release(body: &str) -> Result<Release, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("unreadable release list: {error}"))?;
    let tag = value["tag_name"].as_str().unwrap_or_default().trim().to_string();
    if tag.is_empty() {
        return Err("GitHub's answer has no release tag".to_string());
    }
    let page = value["html_url"].as_str().unwrap_or_default().to_string();
    let suffix = asset_suffix();
    let asset = suffix.and_then(|suffix| {
        value["assets"].as_array()?.iter().find_map(|asset| {
            let name = asset["name"].as_str()?;
            if !name.ends_with(suffix) {
                return None;
            }
            Some(Asset {
                name: name.to_string(),
                url: asset["browser_download_url"].as_str()?.to_string(),
                size: asset["size"].as_u64().unwrap_or(0),
            })
        })
    });
    Ok(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        tag,
        page,
        asset,
    })
}

/// Is `candidate` a later version than `current`? Versions are compared field
/// by field, so `0.1.10` beats `0.1.9` (a string compare would say otherwise).
/// A pre-release suffix (`-beta.2`) is not treated as newer than the release.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    fn fields(version: &str) -> Vec<u64> {
        // Only the release part counts: `0.1.5-beta.1` is the same release as
        // `0.1.5`, not a later one, so the tag's suffix is dropped whole.
        let core = version
            .trim_start_matches(['v', 'V'])
            .split(['-', '+'])
            .next()
            .unwrap_or("");
        core.split('.')
            .map(|part| part.trim().parse::<u64>().unwrap_or(0))
            .collect()
    }
    let (new, old) = (fields(candidate), fields(current));
    for index in 0..new.len().max(old.len()) {
        let new = new.get(index).copied().unwrap_or(0);
        let old = old.get(index).copied().unwrap_or(0);
        if new != old {
            return new > old;
        }
    }
    false
}

/// Download `asset` into `dir`, reporting progress as `(received, total)`.
///
/// The bytes go to a `.part` file first, so a half-finished download is never
/// mistaken for an archive. `cancel` is checked between chunks.
pub fn download(
    asset: &Asset,
    dir: &Path,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let archive = dir.join(&asset.name);
    let partial = dir.join(format!("{}.part", asset.name));

    let response = agent()
        .get(&asset.url)
        .call()
        .map_err(|error| describe(error, "download the archive"))?;
    let total = response
        .header("Content-Length")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|size| *size > 0)
        .unwrap_or(asset.size);

    let mut reader = response.into_reader();
    let mut file =
        File::create(&partial).map_err(|error| format!("could not write {}: {error}", partial.display()))?;
    let mut buffer = [0u8; 64 * 1024];
    let mut received = 0u64;
    progress(received, total);
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(&partial);
            return Err("Download cancelled".to_string());
        }
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("the download stopped: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("could not write the archive: {error}"))?;
        received += read as u64;
        progress(received, total);
    }
    file.flush()
        .map_err(|error| format!("could not finish the archive: {error}"))?;
    drop(file);

    if received == 0 {
        let _ = fs::remove_file(&partial);
        return Err("the download was empty".to_string());
    }
    if total > 0 && received != total {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "the download stopped at {} of {}",
            crate::theme::format_bytes(received),
            crate::theme::format_bytes(total)
        ));
    }
    fs::rename(&partial, &archive)
        .map_err(|error| format!("could not keep {}: {error}", archive.display()))?;
    Ok(archive)
}

/// Put the downloaded archive in place of the running executable and start the
/// new build, through a script that waits for this process to exit.
///
/// The caller is expected to quit the window once this returns; until then the
/// script simply waits.
pub fn apply(archive: &Path) -> Result<(), String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not find this executable: {error}"))?;
    let pid = std::process::id();
    let dir = archive.parent().unwrap_or_else(|| Path::new("."));
    if !archive.is_file() {
        return Err(format!("{} is no longer there", archive.display()));
    }
    let log = dir.join("update.log");
    let script = dir.join(format!("install-{pid}{}", script_suffix()));
    // Scripts from earlier updates have done their work (or died trying); one
    // per update would otherwise pile up in this folder.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("install-") {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    fs::write(&script, script_body(archive, &exe, pid, &log))
        .map_err(|error| format!("could not write {}: {error}", script.display()))?;
    spawn_script(&script)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .user_agent(&format!("disk-scanner/{VERSION}"))
        .build()
}

fn get_text(url: &str, api: bool) -> Result<String, String> {
    let mut request = agent().get(url);
    if api {
        request = request.set("Accept", "application/vnd.github+json");
    }
    let response = request
        .call()
        .map_err(|error| describe(error, "reach GitHub"))?;
    response
        .into_string()
        .map_err(|error| format!("GitHub's answer could not be read: {error}"))
}

/// One sentence for the status bar, instead of ureq's error enum.
fn describe(error: ureq::Error, doing: &str) -> String {
    match error {
        ureq::Error::Status(403 | 429, _) => {
            "GitHub is rate-limiting update checks; try again later".to_string()
        }
        ureq::Error::Status(404, _) => "no release has been published yet".to_string(),
        ureq::Error::Status(status, _) => format!("GitHub answered {status} while trying to {doing}"),
        ureq::Error::Transport(transport) => {
            format!("could not {doing} ({transport})")
        }
    }
}

fn script_suffix() -> &'static str {
    if cfg!(windows) {
        ".ps1"
    } else {
        ".sh"
    }
}

fn spawn_script(script: &Path) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 0x0800_0000 keeps the helper from flashing a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("powershell")
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(script)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|error| format!("could not start the installer: {error}"))?;
    }
    #[cfg(not(windows))]
    {
        use std::process::Stdio;
        std::process::Command::new("sh")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start the installer: {error}"))?;
    }
    Ok(())
}

fn script_body(archive: &Path, exe: &Path, pid: u32, log: &Path) -> String {
    if cfg!(windows) {
        windows_script(archive, exe, pid, log)
    } else {
        unix_script(archive, exe, pid, log)
    }
}

/// A PowerShell script: wait for the app to go, unpack the archive, copy the
/// binary over the old one, start it again. Failures land in `log`, because
/// there is no console left to print them on.
///
/// `ZipFile::ExtractToDirectory` rather than `Expand-Archive`: the cmdlet calls
/// a three-argument overload that older Windows PowerShell builds do not have.
#[cfg(windows)]
fn windows_script(archive: &Path, exe: &Path, pid: u32, log: &Path) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$log = {log}
try {{
    Wait-Process -Id {pid} -ErrorAction SilentlyContinue
    Start-Sleep -Milliseconds 400
    $work = Join-Path $env:TEMP 'disk-scanner-update-{pid}'
    if (Test-Path -LiteralPath $work) {{ Remove-Item -LiteralPath $work -Recurse -Force }}
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::ExtractToDirectory({archive}, $work)
    $binary = Get-ChildItem -LiteralPath $work -Recurse -File -Filter 'disk-scanner.exe' | Select-Object -First 1
    if (-not $binary) {{ throw 'the archive does not contain disk-scanner.exe' }}
    Copy-Item -LiteralPath $binary.FullName -Destination {exe} -Force
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    Remove-Item -LiteralPath {archive} -ErrorAction SilentlyContinue
    Start-Process -FilePath {exe}
}} catch {{
    '{{0}} — {{1}}' -f (Get-Date -Format s), $_ | Out-File -LiteralPath $log -Append -Encoding utf8
    # Never leave the reader without a window: the old binary is still here
    # unless the swap itself failed halfway.
    if (Test-Path -LiteralPath {exe}) {{ Start-Process -FilePath {exe} }}
}}"#,
        log = ps_quote(log),
        pid = pid,
        archive = ps_quote(archive),
        exe = ps_quote(exe),
    )
}

#[cfg(not(windows))]
fn windows_script(_archive: &Path, _exe: &Path, _pid: u32, _log: &Path) -> String {
    String::new()
}

/// The POSIX equivalent: wait on the pid, unpack with `tar`, copy, relaunch.
#[cfg(not(windows))]
fn unix_script(archive: &Path, exe: &Path, pid: u32, log: &Path) -> String {
    format!(
        r#"#!/bin/sh
LOG={log}
{{
  while kill -0 {pid} 2>/dev/null; do sleep 0.2; done
  sleep 0.4
  work=$(mktemp -d)
  tar -xzf {archive} -C "$work"
  binary=$(find "$work" -type f -name disk-scanner | head -n 1)
  if [ -z "$binary" ]; then echo "the archive does not contain disk-scanner"; exit 1; fi
  cp "$binary" {exe}
  chmod +x {exe}
  rm -rf "$work"
  rm -f {archive}
  nohup {exe} >/dev/null 2>&1 &
}} >>"$LOG" 2>&1
"#,
        log = sh_quote(log),
        pid = pid,
        archive = sh_quote(archive),
        exe = sh_quote(exe),
    )
}

#[cfg(windows)]
fn unix_script(_archive: &Path, _exe: &Path, _pid: u32, _log: &Path) -> String {
    String::new()
}

/// A POSIX single-quoted string: the only character it cannot carry is `'`.
#[cfg(not(windows))]
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', r"'\''"))
}

#[cfg(windows)]
#[allow(dead_code)]
fn sh_quote(_path: &Path) -> String {
    String::new()
}

/// A PowerShell single-quoted string: `'` doubles to escape itself.
#[cfg(windows)]
fn ps_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[cfg(not(windows))]
fn ps_quote(_path: &Path) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        "tag_name": "v0.1.5",
        "html_url": "https://github.com/spajce/disk-scanner/releases/tag/v0.1.5",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": "disk-scanner-v0.1.5-windows-x64.zip",
                "browser_download_url": "https://github.com/spajce/disk-scanner/releases/download/v0.1.5/disk-scanner-v0.1.5-windows-x64.zip",
                "size": 3059936
            },
            {
                "name": "disk-scanner-v0.1.5-linux-x64.tar.gz",
                "browser_download_url": "https://example.invalid/linux.tar.gz",
                "size": 6208375
            }
        ]
    }"#;

    #[test]
    fn a_newer_release_is_newer() {
        assert!(is_newer("0.1.5", "0.1.4"));
        assert!(is_newer("v0.1.5", "0.1.4"));
        assert!(is_newer("0.2", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.4", "0.1.4"));
        assert!(!is_newer("0.1.3", "0.1.4"));
        assert!(!is_newer("0.1.5-beta.1", "0.1.5"));
    }

    #[test]
    fn the_release_is_read_down_to_its_fields() {
        let release = parse_release(SAMPLE).expect("sample should parse");
        assert_eq!(release.version, "0.1.5");
        assert_eq!(release.tag, "v0.1.5");
        assert!(release.page.ends_with("/tag/v0.1.5"));
        match asset_suffix() {
            Some(suffix) => {
                let asset = release.asset.expect("this platform's asset");
                assert!(asset.name.ends_with(suffix), "{} vs {suffix}", asset.name);
                assert_eq!(asset.size, 3_059_936);
            }
            // A platform without a published build still reads the release.
            None => assert!(release.asset.is_none()),
        }
    }

    #[test]
    fn a_release_without_a_tag_is_an_error() {
        assert!(parse_release(r#"{"assets": []}"#).is_err());
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn the_installer_script_waits_for_this_process() {
        let archive = Path::new("/tmp/a/disk-scanner.zip");
        let exe = Path::new("/tmp/app/disk-scanner.exe");
        let log = Path::new("/tmp/a/update.log");
        let script = script_body(archive, exe, 4242, log);
        if cfg!(windows) {
            assert!(script.contains("Wait-Process -Id 4242"));
            assert!(script.contains("ZipFile]::ExtractToDirectory"));
            assert!(script.contains("Start-Process"));
            assert!(script.contains("'C:\\tmp\\app\\disk-scanner.exe'") || script.contains("disk-scanner.exe"));
        } else {
            assert!(script.contains("kill -0 4242"));
            assert!(script.contains("tar -xzf"));
        }
    }
}

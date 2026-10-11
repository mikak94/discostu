//! Self-update from GitHub releases.
//!
//! CI publishes every master build as release `build-N` with `discostu.exe`
//! attached, and bakes N into that exe. On launch a CI build asks GitHub for
//! the latest release; if it's newer, it downloads it next to itself and
//! swaps it in (Windows lets a running exe be renamed, just not replaced).
//! The new build runs from the next start, or right away via Restart.
//! Local builds have no number and never update themselves.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use serde::Deserialize;

const REPO: &str = "mikak94/discostu";
const ASSET: &str = "discostu.exe";
/// Passed to the restarted exe: wait for this process id to exit first.
const AFTER_ARG: &str = "--after-update";

/// The CI run that built us; `None` for a local build.
pub fn build() -> Option<u32> {
    option_env!("DISCOSTU_BUILD")?.parse().ok()
}

/// Build staged on disk, waiting for a restart (0 = none).
static READY: AtomicU32 = AtomicU32::new(0);
static RESTART: AtomicBool = AtomicBool::new(false);

pub fn ready() -> Option<u32> {
    Some(READY.load(Ordering::Relaxed)).filter(|&n| n != 0)
}

/// Checks for a newer build in the background and stages it.
pub fn spawn() {
    let Some(current) = build() else { return };
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        let Ok(exe) = std::env::current_exe() else { return };
        // The build we replaced last time, no longer running.
        let _ = fs::remove_file(sibling(&exe, "old"));
        match stage(&exe, current) {
            Ok(Some(n)) => READY.store(n, Ordering::Relaxed),
            Ok(None) => {}
            Err(e) => eprintln!("update: {e}"),
        }
    });
}

/// Asks [`finish`] to start the new build once the UI has closed.
pub fn request_restart() {
    RESTART.store(true, Ordering::Relaxed);
}

/// After shutdown: launches the (updated) exe if a restart was requested.
pub fn finish() {
    if !RESTART.load(Ordering::Relaxed) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(i) = args.iter().position(|a| a == AFTER_ARG) {
        args.drain(i..(i + 2).min(args.len()));
    }
    args.extend([AFTER_ARG.to_string(), std::process::id().to_string()]);
    if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
        eprintln!("update: restart failed: {e}");
    }
}

/// In the restarted exe: lets the old process exit first (it holds the
/// single-instance lock and the network port).
pub fn wait_for_previous(args: &[String]) {
    let Some(pid) = args.iter().position(|a| a == AFTER_ARG).and_then(|i| args.get(i + 1)?.parse::<u32>().ok())
    else {
        return;
    };
    #[cfg(windows)]
    unsafe {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, 10_000);
            let _ = CloseHandle(h);
        }
    }
    #[cfg(not(windows))]
    let _ = pid;
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".");
    name.push(suffix);
    exe.with_file_name(name)
}

/// Downloads and swaps in a newer build; returns its number.
fn stage(exe: &Path, current: u32) -> Result<Option<u32>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .user_agent(concat!("discostu/", env!("CARGO_PKG_VERSION")))
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(20)))
        .build()
        .into();
    let release: Release = agent
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .call()
        .and_then(|mut r| r.body_mut().read_to_string())
        .map_err(|e| e.to_string())
        .and_then(|body| serde_json::from_str(&body).map_err(|e| e.to_string()))
        .map_err(|e| format!("checking: {e}"))?;
    let Some(latest) = release.tag_name.strip_prefix("build-").and_then(|n| n.parse::<u32>().ok()) else {
        return Ok(None);
    };
    if latest <= current {
        return Ok(None);
    }
    let asset = release.assets.iter().find(|a| a.name == ASSET).ok_or("release has no exe")?;

    let part = sibling(exe, "download");
    let download = || -> Result<(), String> {
        let mut resp = agent.get(&asset.browser_download_url).call().map_err(|e| e.to_string())?;
        let mut file = fs::File::create(&part).map_err(|e| e.to_string())?;
        let n = io::copy(&mut resp.body_mut().as_reader(), &mut file).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        let mut head = [0u8; 2];
        io::Read::read_exact(&mut fs::File::open(&part).map_err(|e| e.to_string())?, &mut head)
            .map_err(|e| e.to_string())?;
        if n != asset.size || &head != b"MZ" {
            return Err(format!("got {n} of {} bytes", asset.size));
        }
        Ok(())
    };
    if let Err(e) = download() {
        let _ = fs::remove_file(&part);
        return Err(format!("downloading build {latest}: {e}"));
    }

    let old = sibling(exe, "old");
    let _ = fs::remove_file(&old);
    let swap = fs::rename(exe, &old).and_then(|()| {
        fs::rename(&part, exe).inspect_err(|_| {
            let _ = fs::rename(&old, exe);
        })
    });
    if let Err(e) = swap {
        let _ = fs::remove_file(&part);
        return Err(format!("installing build {latest}: {e}"));
    }
    Ok(Some(latest))
}

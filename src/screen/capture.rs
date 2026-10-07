//! Screen capture backends.
//!
//! Uses xcap's change-driven recorders (DXGI Desktop Duplication on Windows,
//! ScreenCaptureKit on macOS, PipeWire/X11 on Linux), so an idle desktop
//! costs nothing and a changed one is delivered as soon as the compositor
//! presents it.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use super::codec::{Cursor, Frame};
use crate::clock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    pub primary: bool,
}

impl std::fmt::Display for MonitorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} · {}×{}", self.name, self.width, self.height)
    }
}

pub fn monitors() -> Vec<MonitorInfo> {
    xcap::Monitor::all()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|m| {
            Some(MonitorInfo {
                id: m.id().ok()?,
                name: m.friendly_name().or_else(|_| m.name()).unwrap_or_else(|_| "Display".into()),
                width: m.width().ok()?,
                height: m.height().ok()?,
                x: m.x().unwrap_or(0),
                y: m.y().unwrap_or(0),
                primary: m.is_primary().unwrap_or(false),
            })
        })
        .collect()
}

type Recorder = (xcap::VideoRecorder, Receiver<xcap::Frame>);

/// Recorders are created once per monitor and parked between shares:
/// desktop duplication may only be opened a limited number of times.
static PARKED: LazyLock<Mutex<HashMap<u32, Recorder>>> = LazyLock::new(Default::default);

pub struct Capture {
    monitor: MonitorInfo,
    recorder: Option<Recorder>,
}

impl Capture {
    pub fn open(monitor: &MonitorInfo) -> Result<Self, String> {
        let parked = PARKED.lock().ok().and_then(|mut p| p.remove(&monitor.id));
        let recorder = match parked {
            Some(r) => r,
            None => {
                let m = xcap::Monitor::all()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .find(|m| m.id().ok() == Some(monitor.id))
                    .ok_or("monitor disappeared")?;
                m.video_recorder().map_err(|e| e.to_string())?
            }
        };
        // Drop anything buffered from a previous session.
        while recorder.1.try_recv().is_ok() {}
        recorder.0.start().map_err(|e| e.to_string())?;
        Ok(Self { monitor: monitor.clone(), recorder: Some(recorder) })
    }

    /// Waits up to `timeout` for the next changed frame.
    pub fn next(&mut self, timeout: Duration) -> Result<Option<Frame>, String> {
        let (_, rx) = self.recorder.as_ref().expect("recorder present until drop");
        match rx.recv_timeout(timeout) {
            Ok(f) => {
                // Skip ahead to the newest frame if several queued up.
                let mut f = f;
                while let Ok(newer) = rx.try_recv() {
                    f = newer;
                }
                let (w, h) = (f.width as usize, f.height as usize);
                if f.raw.len() < w * h * 4 {
                    return Ok(None);
                }
                Ok(Some(Frame { width: w, height: h, rgba: f.raw, captured_us: clock::now_us() }))
            }
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => Err("capture stopped".into()),
        }
    }

    /// Cursor position relative to the captured monitor.
    pub fn cursor(&self) -> Cursor {
        match cursor_position() {
            Some((x, y, visible)) => Cursor { x: x - self.monitor.x, y: y - self.monitor.y, visible },
            None => Cursor::default(),
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(rec) = self.recorder.take() {
            let _ = rec.0.stop();
            if let Ok(mut p) = PARKED.lock() {
                p.insert(self.monitor.id, rec);
            }
        }
    }
}

#[cfg(windows)]
fn cursor_position() -> Option<(i32, i32, bool)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, GetCursorInfo};
    let mut info: CURSORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<CURSORINFO>() as u32;
    if unsafe { GetCursorInfo(&mut info) } == 0 {
        return None;
    }
    Some((info.ptScreenPos.x, info.ptScreenPos.y, info.flags & CURSOR_SHOWING != 0))
}

#[cfg(not(windows))]
fn cursor_position() -> Option<(i32, i32, bool)> {
    // macOS/Linux recorders composite the cursor into the frame themselves.
    None
}

//! Fallback share path (no hardware encoder, or non-Windows): xcap capture →
//! lossless/lossy tile diff → per-viewer TCP writers.
//!
//! Each viewer has its own writer thread that always sends the *newest*
//! frame, diffed against exactly what that viewer already has.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::net::quic::BlockingSend;
use crate::protocol::{self, ShareInfo};
use super::capture::{Capture, MonitorInfo};
use super::codec::{self, AnalyzedFrame, Cursor};

// ---------------------------------------------------------------------------
// Sharing side
// ---------------------------------------------------------------------------

struct Latest {
    frame: Option<Arc<AnalyzedFrame>>,
    cursor: Cursor,
    cursor_seq: u64,
}

pub struct ShareHub {
    pub info: ShareInfo,
    latest: Mutex<Latest>,
    changed: Condvar,
    stop: AtomicBool,
    pub viewers: AtomicUsize,
    pub fps: AtomicU64,
}

impl ShareHub {
    pub fn start(monitor: &MonitorInfo) -> Result<Arc<Self>, String> {
        let mut capture = Capture::open(monitor)?;
        let hub = Arc::new(Self {
            info: ShareInfo {
                width: monitor.width,
                height: monitor.height,
                monitor: monitor.name.clone(),
            },
            latest: Mutex::new(Latest { frame: None, cursor: Cursor::default(), cursor_seq: 0 }),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
            viewers: AtomicUsize::new(0),
            fps: AtomicU64::new(0),
        });
        let h = hub.clone();
        thread::Builder::new()
            .name("screen-capture".into())
            .spawn(move || {
                let _ = thread_priority::set_current_thread_priority(
                    thread_priority::ThreadPriority::Max,
                );
                let mut number = 0u64;
                let mut window_start = Instant::now();
                let mut window_frames = 0u64;
                while !h.stop.load(Ordering::Relaxed) {
                    // Short timeout so cursor movement is forwarded at ~250 Hz
                    // even when the desktop itself is static.
                    let frame = match capture.next(Duration::from_millis(4)) {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("capture: {e}");
                            break;
                        }
                    };
                    let cursor = capture.cursor();
                    let analyzed = frame.map(|f| {
                        number += 1;
                        window_frames += 1;
                        Arc::new(AnalyzedFrame::new(f, number))
                    });
                    {
                        let mut l = h.latest.lock().expect("hub lock");
                        let mut notify = false;
                        if let Some(a) = analyzed {
                            l.frame = Some(a);
                            notify = true;
                        }
                        if cursor != l.cursor {
                            l.cursor = cursor;
                            l.cursor_seq += 1;
                            notify = true;
                        }
                        if notify {
                            h.changed.notify_all();
                        }
                    }
                    if window_start.elapsed() >= Duration::from_secs(1) {
                        h.fps.store(window_frames, Ordering::Relaxed);
                        window_frames = 0;
                        window_start = Instant::now();
                    }
                }
                h.stop.store(true, Ordering::Relaxed);
                h.changed.notify_all();
            })
            .map_err(|e| e.to_string())?;
        Ok(hub)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self.changed.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Serves one viewer until it disconnects or sharing stops.
    pub fn serve(self: &Arc<Self>, mut stream: BlockingSend) {
        self.viewers.fetch_add(1, Ordering::Relaxed);
        let mut known: Vec<u64> = Vec::new();
        let mut sent_frame = 0u64;
        let mut sent_cursor = u64::MAX;
        loop {
            let (frame, cursor, cursor_seq) = {
                let mut l = self.latest.lock().expect("hub lock");
                loop {
                    if self.is_stopped() {
                        break;
                    }
                    let new_frame = l.frame.as_ref().is_some_and(|f| f.number != sent_frame);
                    if new_frame || l.cursor_seq != sent_cursor {
                        break;
                    }
                    l = self.changed.wait_timeout(l, Duration::from_millis(250)).expect("hub lock").0;
                }
                (l.frame.clone(), l.cursor, l.cursor_seq)
            };
            if self.is_stopped() {
                break;
            }
            let result = match frame.filter(|f| f.number != sent_frame) {
                Some(f) => {
                    sent_frame = f.number;
                    sent_cursor = cursor_seq;
                    match f.encode_delta(&mut known, cursor) {
                        Some(msg) => protocol::write_frame(&mut stream, &msg),
                        None => protocol::write_frame(&mut stream, &codec::encode_cursor(cursor)),
                    }
                }
                None => {
                    sent_cursor = cursor_seq;
                    protocol::write_frame(&mut stream, &codec::encode_cursor(cursor))
                }
            };
            if result.and_then(|_| stream.flush()).is_err() {
                break;
            }
        }
        stream.close();
        self.viewers.fetch_sub(1, Ordering::Relaxed);
    }
}


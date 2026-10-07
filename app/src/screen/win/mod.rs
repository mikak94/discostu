//! Windows screen sharing: WGC capture → GPU NV12 → hardware H.264, plus
//! per-process system audio, fanned out to every viewer.
//!
//! Each viewer has a short queue. If a viewer falls behind, its queue is
//! flushed and it waits for the next keyframe (requested immediately),
//! so a slow viewer never adds latency for itself or anyone else.

pub mod capture;
mod convert;
mod d3d;
pub mod decoder;
pub mod encoder;

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};

use crate::audio::loopback::{self, Loopback};
use crate::clock;
use crate::net::Link;
use crate::net::quic::BlockingSend;
use crate::protocol::{self, PeerId, ShareInfo};
use capture::{Target, Wgc};
use d3d::{D3d, SendCell};
use encoder::{EncodedFrame, Encoder};

pub use super::Source;

/// Frames per second we capture and encode at most.
const MAX_FPS: u32 = 120;

struct ViewerQueue {
    id: u64,
    tx: Sender<Arc<EncodedFrame>>,
    waiting_for_key: bool,
    /// Set by the writer when it dropped its backlog.
    resync: Arc<AtomicBool>,
}

pub struct VideoHub {
    pub info: ShareInfo,
    pub encoder: String,
    pub stop: Arc<AtomicBool>,
    pub viewers: AtomicUsize,
    pub fps: AtomicU64,
    pub mbps: Arc<Mutex<f32>>,
    pub audio: bool,
    key_request: Arc<AtomicBool>,
    queues: Arc<Mutex<Vec<ViewerQueue>>>,
    audio_targets: Arc<Mutex<Vec<(u64, Link)>>>,
    next_viewer: AtomicU64,
    _capture: Mutex<Option<SendCell<Wgc>>>,
    _loopback: Mutex<Option<Loopback>>,
}

/// Largest frame we encode (H.264 level limits and sane bandwidth).
fn encode_size(w: u32, h: u32) -> (u32, u32) {
    let scale = (3840.0 / w as f32).min(2160.0 / h as f32).min(1.0);
    let ew = ((w as f32 * scale) as u32 & !1).max(64);
    let eh = ((h as f32 * scale) as u32 & !1).max(64);
    (ew, eh)
}

/// Bitrate for a resolution: plenty for crisp text on a LAN.
fn bitrate(w: u32, h: u32) -> u32 {
    ((w as f64 * h as f64 * 60.0 * 0.15) as u32).clamp(8_000_000, 80_000_000)
}

impl VideoHub {
    pub fn start(source: &Source, share_audio: bool, me: PeerId) -> Result<Arc<Self>, String> {
        let d3d = D3d::new().map_err(|e| format!("Direct3D: {e}"))?;
        let target = match source.kind {
            super::SourceKind::Monitor { handle } => Target::Monitor(handle),
            super::SourceKind::Window { handle, .. } => Target::Window(handle),
        };
        let item_size = capture::item_for(target)
            .and_then(|i| i.Size())
            .map_err(|e| format!("capture: {e}"))?;
        let (ew, eh) = encode_size(item_size.Width.max(64) as u32, item_size.Height.max(64) as u32);
        let converter = convert::Converter::new(&d3d, item_size.Width as u32, item_size.Height as u32, ew, eh)
            .map_err(|e| format!("video processor: {e}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let key_request = Arc::new(AtomicBool::new(false));
        let queues: Arc<Mutex<Vec<ViewerQueue>>> = Arc::new(Mutex::new(Vec::new()));
        let mbps = Arc::new(Mutex::new(0f32));
        let fps_counter = Arc::new(AtomicU64::new(0));

        // Encoder on its own thread (it owns COM objects).
        let (frame_tx, frame_rx) = crossbeam_channel::bounded::<(SendCell<_>, u64)>(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<String, String>>(1);
        {
            let d3d = SendCell(d3d.clone());
            let (stop, key, queues, mbps, fps) =
                (stop.clone(), key_request.clone(), queues.clone(), mbps.clone(), fps_counter.clone());
            thread::Builder::new()
                .name("h264-encoder".into())
                .spawn(move || {
                    let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
                    let d3d = d3d;
                    let enc = match Encoder::new(&d3d.0, ew, eh, 60, bitrate(ew, eh)) {
                        Ok(e) => e,
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("no hardware H.264 encoder: {e}")));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(enc.name.clone()));
                    let (mut bytes, mut frames, mut window) = (0usize, 0u64, Instant::now());
                    enc.run(frame_rx, key.clone(), stop, move |frame| {
                        bytes += frame.data.len();
                        frames += 1;
                        if window.elapsed() >= Duration::from_secs(1) {
                            let secs = window.elapsed().as_secs_f32();
                            *mbps.lock().expect("mbps") = bytes as f32 * 8.0 / secs / 1e6;
                            fps.store((frames as f32 / secs).round() as u64, Ordering::Relaxed);
                            (bytes, frames, window) = (0, 0, Instant::now());
                        }
                        fan_out(&queues, &key, Arc::new(frame));
                    });
                })
                .map_err(|e| e.to_string())?;
        }
        let encoder_name = ready_rx
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| "encoder did not start".to_string())??;

        // Capture → convert on the WGC thread, hand the NV12 texture over.
        let converter = Mutex::new(SendCell(converter));
        let min_interval = Duration::from_micros(1_000_000 / MAX_FPS as u64);
        let last = Mutex::new(Instant::now() - min_interval);
        let cap_stop = stop.clone();
        let (wgc, _) = Wgc::start(&d3d, target, move |tex, cw, ch| {
            if cap_stop.load(Ordering::Relaxed) {
                return;
            }
            let mut l = last.lock().expect("rate lock");
            if l.elapsed() < min_interval {
                return;
            }
            *l = Instant::now();
            drop(l);
            let captured = clock::now_us();
            let nv12 = match converter.lock().expect("converter").0.convert(tex, cw, ch) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("convert: {e}");
                    return;
                }
            };
            // Newest frame wins if the encoder hasn't taken the last one yet.
            if let Err(TrySendError::Full(f)) = frame_tx.try_send((SendCell(nv12), captured)) {
                let _ = frame_tx.try_send(f);
            }
        })
        .map_err(|e| format!("capture: {e}"))?;

        // Watch for the shared window closing.
        {
            let (closed, stop) = (wgc.closed.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if closed.load(Ordering::Relaxed) {
                        stop.store(true, Ordering::Relaxed);
                    }
                    thread::sleep(Duration::from_millis(200));
                }
            });
        }

        let audio_targets: Arc<Mutex<Vec<(u64, Link)>>> = Arc::new(Mutex::new(Vec::new()));
        let loopback = if share_audio {
            let target = match source.kind {
                super::SourceKind::Monitor { .. } => loopback::Target::AllExcept(std::process::id()),
                super::SourceKind::Window { pid, .. } => loopback::Target::Process(pid),
            };
            let targets = audio_targets.clone();
            let mut block = Vec::with_capacity(crate::audio::STEREO_BLOCK);
            let mut packet = Vec::with_capacity(protocol::MAX_DATAGRAM);
            let mut seq = 0u32;
            match Loopback::start(target, move |samples| {
                for &s in samples {
                    // System audio is float and can exceed full scale (loud games,
                    // volume boosts): round peaks off rather than clip them flat.
                    block.push(crate::audio::dsp::soft_limit(s));
                    if block.len() == crate::audio::STEREO_BLOCK {
                        protocol::write_stream(&mut packet, me, seq, &block);
                        seq = seq.wrapping_add(1);
                        for (_, link) in targets.lock().expect("targets").iter() {
                            link.send(&packet);
                        }
                        block.clear();
                    }
                }
            }) {
                Ok(l) => Some(l),
                Err(e) => {
                    eprintln!("system audio capture unavailable: {e}");
                    None
                }
            }
        } else {
            None
        };

        let hub = Arc::new(Self {
            info: ShareInfo { width: ew, height: eh, monitor: source.title.clone() },
            encoder: encoder_name,
            stop,
            viewers: AtomicUsize::new(0),
            fps: AtomicU64::new(0),
            mbps,
            audio: loopback.is_some(),
            key_request,
            queues,
            audio_targets,
            next_viewer: AtomicU64::new(1),
            _capture: Mutex::new(Some(SendCell(wgc))),
            _loopback: Mutex::new(loopback),
        });
        // Mirror the encoder's fps counter into the hub.
        {
            let h = Arc::downgrade(&hub);
            thread::spawn(move || {
                while let Some(hub) = h.upgrade() {
                    if hub.stop.load(Ordering::Relaxed) {
                        break;
                    }
                    hub.fps.store(fps_counter.load(Ordering::Relaxed), Ordering::Relaxed);
                    drop(hub);
                    thread::sleep(Duration::from_millis(500));
                }
            });
        }
        Ok(hub)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        self._capture.lock().expect("capture").take();
        self._loopback.lock().expect("loopback").take();
        self.queues.lock().expect("queues").clear();
    }

    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// Serves one viewer until it disconnects or sharing stops.
    pub fn serve(self: &Arc<Self>, mut stream: BlockingSend, audio_to: Option<Link>) {
        let id = self.next_viewer.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = crossbeam_channel::bounded::<Arc<EncodedFrame>>(6);
        let resync = Arc::new(AtomicBool::new(false));
        self.queues.lock().expect("queues").push(ViewerQueue { id, tx, waiting_for_key: true, resync: resync.clone() });
        self.key_request.store(true, Ordering::Relaxed);
        if let Some(link) = audio_to {
            self.audio_targets.lock().expect("targets").push((id, link));
        }
        self.viewers.fetch_add(1, Ordering::Relaxed);

        while !self.is_stopped() {
            let Ok(frame) = rx.recv_timeout(Duration::from_millis(250)) else { continue };
            if rx.len() >= 3 {
                // Network can't keep up: drop the backlog and resync on a keyframe.
                while rx.try_recv().is_ok() {}
                resync.store(true, Ordering::Relaxed);
                continue;
            }
            let msg = super::codec::encode_h264(&frame);
            if protocol::write_frame(&mut stream, &msg).and_then(|_| stream.flush()).is_err() {
                break;
            }
        }
        self.queues.lock().expect("queues").retain(|q| q.id != id);
        self.audio_targets.lock().expect("targets").retain(|(v, _)| *v != id);
        self.viewers.fetch_sub(1, Ordering::Relaxed);
        stream.close();
    }
}

fn fan_out(queues: &Mutex<Vec<ViewerQueue>>, key_request: &AtomicBool, frame: Arc<EncodedFrame>) {
    let mut qs = queues.lock().expect("queues");
    for q in qs.iter_mut() {
        if q.resync.swap(false, Ordering::Relaxed) {
            q.waiting_for_key = true;
            key_request.store(true, Ordering::Relaxed);
        }
        if q.waiting_for_key {
            if !frame.key {
                continue;
            }
            q.waiting_for_key = false;
        }
        match q.tx.try_send(frame.clone()) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // Too far behind: stop feeding until a fresh keyframe.
                q.waiting_for_key = true;
                key_request.store(true, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

fn is_cloaked(hwnd: isize) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            windows::Win32::Foundation::HWND(hwnd as _),
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut _,
            4,
        )
        .is_ok()
            && cloaked != 0
    }
}

pub fn sources() -> Vec<Source> {
    let mut out = Vec::new();
    for m in xcap::Monitor::all().unwrap_or_default() {
        let (Ok(x), Ok(y), Ok(w), Ok(h)) = (m.x(), m.y(), m.width(), m.height()) else { continue };
        let center = POINT { x: x + w as i32 / 2, y: y + h as i32 / 2 };
        let handle = unsafe { MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST) }.0 as isize;
        let name = m.friendly_name().or_else(|_| m.name()).unwrap_or_else(|_| "Display".into());
        let primary = m.is_primary().unwrap_or(false);
        out.push(Source {
            kind: super::SourceKind::Monitor { handle },
            title: name,
            subtitle: format!("{w}×{h}{}", if primary { " · primary" } else { "" }),
            width: w,
            height: h,
            xcap_id: m.id().unwrap_or(0),
        });
    }
    let me = std::process::id();
    let mut windows: Vec<(i32, Source)> = xcap::Window::all()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|w| {
            let id = w.id().ok()?;
            let pid = w.pid().ok()?;
            let title = w.title().ok()?;
            let (width, height) = (w.width().ok()?, w.height().ok()?);
            let handle = id as isize;
            if pid == me || title.trim().is_empty() || width < 120 || height < 80 || is_cloaked(handle) {
                return None;
            }
            let app = w.app_name().unwrap_or_default();
            if app.eq_ignore_ascii_case("explorer") && title == "Program Manager" {
                return None;
            }
            let minimized = w.is_minimized().unwrap_or(false);
            Some((
                w.z().unwrap_or(0),
                Source {
                    kind: super::SourceKind::Window { handle, pid },
                    title,
                    subtitle: if minimized { format!("{app} · minimized") } else { app },
                    width,
                    height,
                    xcap_id: id,
                },
            ))
        })
        .collect();
    windows.sort_by_key(|(z, _)| std::cmp::Reverse(*z));
    out.extend(windows.into_iter().map(|(_, s)| s));
    out
}

/// Small RGBA preview (max 320 px wide) for the picker.
pub fn thumbnail(source: &Source) -> Option<(u32, u32, Vec<u8>)> {
    let img = match source.kind {
        super::SourceKind::Monitor { .. } => xcap::Monitor::all()
            .ok()?
            .into_iter()
            .find(|m| m.id().ok() == Some(source.xcap_id))?
            .capture_image()
            .ok()?,
        super::SourceKind::Window { .. } => xcap::Window::all()
            .ok()?
            .into_iter()
            .find(|w| w.id().ok() == Some(source.xcap_id))?
            .capture_image()
            .ok()?,
    };
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let tw = 320.min(w);
    let th = ((h as u64 * tw as u64) / w as u64).max(1) as u32;
    let src = img.as_raw();
    let mut out = vec![0u8; (tw * th * 4) as usize];
    // Box filter: average the source pixels covering each target pixel.
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let mut acc = [0u32; 4];
            let mut n = 0u32;
            for y in (y0..y1).step_by(2.max(((y1 - y0) / 4) as usize)) {
                for x in (x0..x1).step_by(2.max(((x1 - x0) / 4) as usize)) {
                    let i = ((y * w + x) * 4) as usize;
                    for c in 0..4 {
                        acc[c] += src[i + c] as u32;
                    }
                    n += 1;
                }
            }
            let o = ((ty * tw + tx) * 4) as usize;
            for c in 0..4 {
                out[o + c] = (acc[c] / n.max(1)) as u8;
            }
            out[o + 3] = 255;
        }
    }
    Some((tw, th, out))
}

//! Screen sharing.
//!
//! - **Windows**: WGC capture (screens or single windows) → GPU NV12 →
//!   hardware H.264 (`win`), with the shared app's/system audio alongside.
//! - **Fallback** (no hardware encoder, other OSes): xcap capture → tile
//!   codec (`tiles`).
//!
//! Viewers decode into a [`VideoSink`] that the GPU widget uploads from.

pub mod capture;
pub mod codec;
mod tiles;
#[cfg(windows)]
pub mod win;

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use crate::clock;
use crate::net::Link;
use crate::net::quic::{BlockingRecv, BlockingSend};
use crate::protocol::{self, PeerId, ShareInfo};
use codec::{Cursor, Message, Rect};
pub use tiles::ShareHub;

#[derive(Debug, Clone, PartialEq)]
pub enum SourceKind {
    Monitor { handle: isize },
    Window { handle: isize, pid: u32 },
}

/// Something that can be shared: a screen or (on Windows) a window.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub kind: SourceKind,
    pub title: String,
    pub subtitle: String,
    pub width: u32,
    pub height: u32,
    pub xcap_id: u32,
}

impl Source {
    pub fn is_window(&self) -> bool {
        matches!(self.kind, SourceKind::Window { .. })
    }
}

pub fn sources() -> Vec<Source> {
    #[cfg(windows)]
    {
        win::sources()
    }
    #[cfg(not(windows))]
    {
        capture::monitors()
            .into_iter()
            .map(|m| Source {
                kind: SourceKind::Monitor { handle: m.id as isize },
                subtitle: format!("{}×{}", m.width, m.height),
                title: m.name,
                width: m.width,
                height: m.height,
                xcap_id: m.id,
            })
            .collect()
    }
}

pub fn thumbnail(source: &Source) -> Option<(u32, u32, Vec<u8>)> {
    #[cfg(windows)]
    {
        win::thumbnail(source)
    }
    #[cfg(not(windows))]
    {
        let _ = source;
        None
    }
}

/// An active share, whichever pipeline carries it.
#[derive(Clone)]
pub enum Hub {
    Tiles(Arc<ShareHub>),
    #[cfg(windows)]
    Video(Arc<win::VideoHub>),
}

#[derive(Debug, Clone, Default)]
pub struct HubStats {
    pub viewers: usize,
    pub fps: u64,
    pub mbps: f32,
    pub codec: String,
    pub audio: bool,
}

impl Hub {
    /// Starts sharing. Returns a warning when it had to fall back.
    pub fn start(
        source: &Source,
        share_audio: bool,
        me: PeerId,
    ) -> Result<(Hub, Option<String>), String> {
        #[cfg(windows)]
        {
            match win::VideoHub::start(source, share_audio, me) {
                Ok(h) => return Ok((Hub::Video(h), None)),
                Err(e) if source.is_window() => return Err(e),
                Err(e) => {
                    eprintln!("hardware pipeline unavailable ({e}); using tile codec");
                    let hub = Self::start_tiles(source)?;
                    return Ok((hub, Some(format!("Hardware encoding unavailable ({e}) — using the tile codec"))));
                }
            }
        }
        #[cfg(not(windows))]
        {
            let _ = (share_audio, me);
            Ok((Self::start_tiles(source)?, None))
        }
    }

    fn start_tiles(source: &Source) -> Result<Hub, String> {
        let monitor = capture::monitors()
            .into_iter()
            .find(|m| m.id == source.xcap_id)
            .ok_or("display not found")?;
        Ok(Hub::Tiles(ShareHub::start(&monitor)?))
    }

    pub fn info(&self) -> ShareInfo {
        match self {
            Hub::Tiles(h) => h.info.clone(),
            #[cfg(windows)]
            Hub::Video(h) => h.info.clone(),
        }
    }

    pub fn stop(&self) {
        match self {
            Hub::Tiles(h) => h.stop(),
            #[cfg(windows)]
            Hub::Video(h) => h.stop(),
        }
    }

    pub fn is_stopped(&self) -> bool {
        match self {
            Hub::Tiles(h) => h.is_stopped(),
            #[cfg(windows)]
            Hub::Video(h) => h.is_stopped(),
        }
    }

    pub fn stats(&self) -> HubStats {
        match self {
            Hub::Tiles(h) => HubStats {
                viewers: h.viewers.load(Ordering::Relaxed),
                fps: h.fps.load(Ordering::Relaxed),
                mbps: 0.0,
                codec: "tiles".into(),
                audio: false,
            },
            #[cfg(windows)]
            Hub::Video(h) => HubStats {
                viewers: h.viewers.load(Ordering::Relaxed),
                fps: h.fps.load(Ordering::Relaxed),
                mbps: *h.mbps.lock().expect("mbps"),
                codec: h.encoder.clone(),
                audio: h.audio,
            },
        }
    }

    pub fn serve(&self, stream: BlockingSend, audio_to: Option<Link>) {
        match self {
            Hub::Tiles(h) => {
                let _ = audio_to;
                h.serve(stream)
            }
            #[cfg(windows)]
            Hub::Video(h) => h.serve(stream, audio_to),
        }
    }
}

// ---------------------------------------------------------------------------
// Viewing side
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct VideoStats {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    /// Capture on the sharer → decoded here, using the ping clock offset.
    pub latency_ms: f32,
    /// Sharer-side capture → encoded bitstream.
    pub encode_ms: f32,
    /// Decode time here.
    pub decode_ms: f32,
    pub mbps: f32,
    pub codec: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PixelFormat {
    #[default]
    Rgba,
    /// Tight NV12: `width*height` luma then `width*height/2` interleaved chroma.
    Nv12,
}

pub struct SinkFrame {
    pub format: PixelFormat,
    pub width: usize,
    pub height: usize,
    pub data: Vec<u8>,
    /// Regions changed since the GPU last uploaded (RGBA tiles).
    pub dirty: Vec<Rect>,
    pub full_dirty: bool,
    pub cursor: Cursor,
    /// Bumps when format or dimensions change so textures are recreated.
    pub epoch: u64,
}

impl SinkFrame {
    fn reshape(&mut self, format: PixelFormat, w: usize, h: usize) {
        if self.format != format || self.width != w || self.height != h {
            self.format = format;
            self.width = w;
            self.height = h;
            self.data = vec![0; if format == PixelFormat::Nv12 { w * h * 3 / 2 } else { w * h * 4 }];
            self.epoch += 1;
            self.full_dirty = true;
            self.dirty.clear();
        }
    }
}

pub struct VideoSink {
    pub id: u64,
    pub frame: Mutex<SinkFrame>,
    pub stats: Mutex<VideoStats>,
    pub connected: AtomicBool,
    pub has_frame: AtomicBool,
}

impl std::fmt::Debug for VideoSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VideoSink({})", self.id)
    }
}

static SINK_IDS: AtomicU64 = AtomicU64::new(1);

impl VideoSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            id: SINK_IDS.fetch_add(1, Ordering::Relaxed),
            frame: Mutex::new(SinkFrame {
                format: PixelFormat::Rgba,
                width: 0,
                height: 0,
                data: Vec::new(),
                dirty: Vec::new(),
                full_dirty: false,
                cursor: Cursor::default(),
                epoch: 0,
            }),
            stats: Mutex::new(VideoStats::default()),
            connected: AtomicBool::new(true),
            has_frame: AtomicBool::new(false),
        })
    }
}

pub struct Viewer {
    pub sink: Arc<VideoSink>,
    conn: quinn::Connection,
    /// Held open for the life of the view (closing it ends the stream).
    _send: quinn::SendStream,
}

impl Viewer {
    /// Reads a sharer's screen stream on a connection `net::quic::open_screen`
    /// set up. `clock_offset_us` is the sharer's clock minus ours, kept
    /// current by the ping loop.
    pub fn start(
        conn: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        clock_offset_us: Arc<AtomicI64>,
    ) -> Result<Self, String> {
        let sink = VideoSink::new();
        let reader = BlockingRecv::new(recv);
        let s = sink.clone();
        thread::Builder::new()
            .name("screen-recv".into())
            .spawn(move || {
                let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
                receive(reader, s, clock_offset_us)
            })
            .map_err(|e| e.to_string())?;
        Ok(Self { sink, conn, _send: send })
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"stopped watching");
    }
}

struct Meter {
    window_start: Instant,
    frames: u32,
    bytes: usize,
    latency: f32,
    encode: f32,
    decode: f32,
}

impl Meter {
    fn smooth(avg: &mut f32, v: f32) {
        *avg = if *avg == 0.0 { v } else { *avg * 0.8 + v * 0.2 };
    }
}

fn receive(mut stream: BlockingRecv, sink: Arc<VideoSink>, offset: Arc<AtomicI64>) {
    let mut buf = Vec::new();
    let mut m = Meter { window_start: Instant::now(), frames: 0, bytes: 0, latency: 0.0, encode: 0.0, decode: 0.0 };
    let mut codec_name = "tiles";
    #[cfg(windows)]
    let mut decoder: Option<win::decoder::Decoder> = None;
    #[cfg(windows)]
    let mut waiting_for_key = true;

    while protocol::read_frame(&mut stream, &mut buf, 256 << 20).is_ok() {
        m.bytes += buf.len() + 4;
        let mut captured = None;
        match codec::parse(&buf) {
            Some(Message::Frame(hdr, tiles)) => {
                apply_tiles(&sink, hdr.width, hdr.height, &tiles, hdr.cursor);
                captured = Some(hdr.captured_us);
            }
            Some(Message::Cursor(c)) => {
                sink.frame.lock().expect("sink lock").cursor = c;
            }
            #[cfg(windows)]
            Some(Message::H264(f)) => {
                codec_name = "H.264";
                if waiting_for_key && !f.key {
                    continue;
                }
                waiting_for_key = false;
                if decoder.is_none() {
                    match win::decoder::Decoder::new() {
                        Ok(d) => decoder = Some(d),
                        Err(e) => {
                            eprintln!("H.264 decoder unavailable: {e}");
                            break;
                        }
                    }
                }
                let started = Instant::now();
                let (w, h) = (f.width, f.height);
                let mut got = false;
                let result = decoder.as_mut().expect("decoder").decode(f.data, |nv12| {
                    got = true;
                    write_nv12(&sink, w, h, &nv12);
                });
                if let Err(e) = result {
                    eprintln!("decode: {e}");
                    decoder = None;
                    waiting_for_key = true;
                    continue;
                }
                if got {
                    Meter::smooth(&mut m.decode, started.elapsed().as_secs_f32() * 1000.0);
                    Meter::smooth(&mut m.encode, f.encode_us as f32 / 1000.0);
                    captured = Some(f.captured_us);
                }
            }
            #[cfg(not(windows))]
            Some(Message::H264(_)) => {
                eprintln!("H.264 streams need the Windows decoder");
                break;
            }
            None => break,
        }
        if let Some(cap) = captured {
            sink.has_frame.store(true, Ordering::Relaxed);
            m.frames += 1;
            let sharer_now = clock::now_us() as i64 + offset.load(Ordering::Relaxed);
            Meter::smooth(&mut m.latency, (sharer_now - cap as i64) as f32 / 1000.0);
        }
        if m.window_start.elapsed() >= Duration::from_secs(1) {
            let secs = m.window_start.elapsed().as_secs_f32();
            let f = sink.frame.lock().expect("sink lock");
            *sink.stats.lock().expect("stats lock") = VideoStats {
                width: f.width,
                height: f.height,
                fps: (m.frames as f32 / secs).round() as u32,
                latency_ms: m.latency.max(0.0),
                encode_ms: m.encode,
                decode_ms: m.decode,
                mbps: m.bytes as f32 * 8.0 / secs / 1e6,
                codec: codec_name,
            };
            drop(f);
            m.frames = 0;
            m.bytes = 0;
            m.window_start = Instant::now();
        }
    }
    sink.connected.store(false, Ordering::Relaxed);
}

fn apply_tiles(sink: &VideoSink, w: usize, h: usize, tiles: &[codec::TileRef<'_>], cursor: Cursor) {
    let decoded: Vec<(Rect, Vec<u8>)> = tiles
        .par_iter()
        .filter_map(|t| {
            let r = codec::tile_rect(w, h, t.tx, t.ty);
            codec::decode_tile(t.data, r.w, r.h).map(|px| (r, px))
        })
        .collect();
    let mut f = sink.frame.lock().expect("sink lock");
    f.reshape(PixelFormat::Rgba, w, h);
    for (r, px) in &decoded {
        for row in 0..r.h {
            let dst = ((r.y + row) * w + r.x) * 4;
            f.data[dst..dst + r.w * 4].copy_from_slice(&px[row * r.w * 4..(row + 1) * r.w * 4]);
        }
        if !f.full_dirty {
            f.dirty.push(*r);
        }
    }
    if f.dirty.len() > 512 {
        f.full_dirty = true;
        f.dirty.clear();
    }
    f.cursor = cursor;
}

#[cfg(windows)]
fn write_nv12(sink: &VideoSink, w: usize, h: usize, src: &win::decoder::Nv12) {
    let (w, h) = (w.min(src.width) & !1, h.min(src.height) & !1);
    let mut f = sink.frame.lock().expect("sink lock");
    f.reshape(PixelFormat::Nv12, w, h);
    let uv_src = src.stride * src.rows;
    let (y_dst, uv_dst) = f.data.split_at_mut(w * h);
    for row in 0..h {
        y_dst[row * w..(row + 1) * w].copy_from_slice(&src.data[row * src.stride..row * src.stride + w]);
    }
    for row in 0..h / 2 {
        let s = uv_src + row * src.stride;
        if s + w <= src.data.len() {
            uv_dst[row * w..(row + 1) * w].copy_from_slice(&src.data[s..s + w]);
        }
    }
    f.full_dirty = true;
    f.cursor = Cursor::default();
}

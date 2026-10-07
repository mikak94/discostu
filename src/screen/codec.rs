//! Tile-based screen codec tuned for LAN latency rather than bandwidth.
//!
//! The frame is cut into 64×64 tiles; only tiles whose hash changed since
//! the viewer last received them are sent. Each tile is compressed on its
//! own (in parallel) with one of:
//!
//! - `ENC_RGB`: lossless LZ4 over packed RGB — text and UI stay pixel-exact.
//! - `ENC_YCOCG`: YCoCg 4:2:0 with left-prediction, then LZ4 — used only
//!   when a tile is photographic (LZ4 can't shrink it much), roughly halving
//!   the bytes for video/photos at a barely visible chroma cost.
//!
//! There is no inter-frame prediction and no lookahead: a frame is decodable
//! the moment it arrives, which keeps encode+decode in the low milliseconds.

use rayon::prelude::*;
use std::sync::OnceLock;

pub const TILE: usize = 64;
pub const MSG_FRAME: u8 = 1;
pub const MSG_CURSOR: u8 = 2;
pub const MSG_H264: u8 = 3;

const ENC_RGB: u8 = 0;
const ENC_YCOCG: u8 = 1;

/// Raw captured frame, tightly packed RGBA.
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub captured_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub fn grid(width: usize, height: usize) -> (usize, usize) {
    (width.div_ceil(TILE), height.div_ceil(TILE))
}

pub fn tile_rect(width: usize, height: usize, tx: usize, ty: usize) -> Rect {
    let x = tx * TILE;
    let y = ty * TILE;
    Rect { x, y, w: TILE.min(width - x), h: TILE.min(height - y) }
}

/// A captured frame plus per-tile hashes and a lazily filled cache of
/// encoded tiles shared by every viewer.
pub struct AnalyzedFrame {
    pub frame: Frame,
    pub number: u64,
    pub cols: usize,
    pub hashes: Vec<u64>,
    encoded: Vec<OnceLock<Vec<u8>>>,
}

impl AnalyzedFrame {
    pub fn new(frame: Frame, number: u64) -> Self {
        let (cols, rows) = grid(frame.width, frame.height);
        let hashes = (0..cols * rows)
            .into_par_iter()
            .map(|i| hash_tile(&frame, tile_rect(frame.width, frame.height, i % cols, i / cols)))
            .collect();
        let encoded = (0..cols * rows).map(|_| OnceLock::new()).collect();
        Self { frame, number, cols, hashes, encoded }
    }

    fn tile(&self, i: usize) -> &[u8] {
        self.encoded[i].get_or_init(|| {
            let r = tile_rect(self.frame.width, self.frame.height, i % self.cols, i / self.cols);
            encode_tile(&self.frame, r)
        })
    }

    /// Builds a frame message with the tiles whose hashes differ from
    /// `known` (all tiles if `known` doesn't match this frame's grid).
    /// Updates `known` to this frame's hashes.
    pub fn encode_delta(&self, known: &mut Vec<u64>, cursor: Cursor) -> Option<Vec<u8>> {
        let full = known.len() != self.hashes.len();
        let changed: Vec<usize> = (0..self.hashes.len())
            .filter(|&i| full || known[i] != self.hashes[i])
            .collect();
        known.clone_from(&self.hashes);
        if changed.is_empty() {
            return None;
        }
        // Compress in parallel; the cache means each tile is compressed once
        // no matter how many viewers need it.
        changed.par_iter().for_each(|&i| {
            self.tile(i);
        });

        let payload: usize = changed.iter().map(|&i| self.tile(i).len() + 8).sum();
        let mut out = Vec::with_capacity(48 + payload);
        out.push(MSG_FRAME);
        out.extend_from_slice(&(self.frame.width as u32).to_le_bytes());
        out.extend_from_slice(&(self.frame.height as u32).to_le_bytes());
        out.extend_from_slice(&(TILE as u16).to_le_bytes());
        out.extend_from_slice(&self.number.to_le_bytes());
        out.extend_from_slice(&self.frame.captured_us.to_le_bytes());
        write_cursor(&mut out, cursor);
        out.extend_from_slice(&(changed.len() as u32).to_le_bytes());
        for &i in &changed {
            let data = self.tile(i);
            out.extend_from_slice(&((i % self.cols) as u16).to_le_bytes());
            out.extend_from_slice(&((i / self.cols) as u16).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
        }
        Some(out)
    }
}

fn write_cursor(out: &mut Vec<u8>, c: Cursor) {
    out.extend_from_slice(&c.x.to_le_bytes());
    out.extend_from_slice(&c.y.to_le_bytes());
    out.push(c.visible as u8);
}

pub fn encode_cursor(c: Cursor) -> Vec<u8> {
    let mut out = vec![MSG_CURSOR];
    write_cursor(&mut out, c);
    out
}

fn hash_tile(frame: &Frame, r: Rect) -> u64 {
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let stride = frame.width * 4;
    for row in r.y..r.y + r.h {
        let start = row * stride + r.x * 4;
        h.update(&frame.rgba[start..start + r.w * 4]);
    }
    h.digest()
}

fn encode_tile(frame: &Frame, r: Rect) -> Vec<u8> {
    let stride = frame.width * 4;
    let mut rgb = Vec::with_capacity(r.w * r.h * 3);
    for row in r.y..r.y + r.h {
        let start = row * stride + r.x * 4;
        for px in frame.rgba[start..start + r.w * 4].chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
    }
    let lossless = lz4_flex::block::compress(&rgb);
    // Photographic content: LZ4 barely helps. Switch to subsampled chroma.
    // Small edge tiles never compress well and cost nothing; keep them exact.
    if lossless.len() * 10 > rgb.len() * 4 && r.w * r.h >= 1024 {
        let planes = ycocg420(&rgb, r.w, r.h);
        let lossy = lz4_flex::block::compress(&planes);
        if lossy.len() < lossless.len() {
            let mut out = Vec::with_capacity(1 + lossy.len());
            out.push(ENC_YCOCG);
            out.extend_from_slice(&lossy);
            return out;
        }
    }
    let mut out = Vec::with_capacity(1 + lossless.len());
    out.push(ENC_RGB);
    out.extend_from_slice(&lossless);
    out
}

fn chroma_dims(w: usize, h: usize) -> (usize, usize) {
    (w.div_ceil(2), h.div_ceil(2))
}

/// Packs Y (full res) + Co/Cg (half res), each row left-predicted.
fn ycocg420(rgb: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (cw, ch) = chroma_dims(w, h);
    let mut y = vec![0u8; w * h];
    let mut co = vec![0i32; cw * ch];
    let mut cg = vec![0i32; cw * ch];
    let mut count = vec![0i32; cw * ch];
    for py in 0..h {
        for px in 0..w {
            let i = (py * w + px) * 3;
            let (r, g, b) = (rgb[i] as i32, rgb[i + 1] as i32, rgb[i + 2] as i32);
            y[py * w + px] = ((r + 2 * g + b + 2) >> 2) as u8;
            let c = (py / 2) * cw + px / 2;
            co[c] += r - b;
            cg[c] += 2 * g - r - b;
            count[c] += 1;
        }
    }
    let mut out = Vec::with_capacity(w * h + 2 * cw * ch);
    left_predict(&y, w, &mut out);
    let to_u8 = |sum: i32, n: i32, div: i32| ((sum / (n * div)) + 128).clamp(0, 255) as u8;
    let co: Vec<u8> = co.iter().zip(&count).map(|(&s, &n)| to_u8(s, n, 2)).collect();
    let cg: Vec<u8> = cg.iter().zip(&count).map(|(&s, &n)| to_u8(s, n, 4)).collect();
    left_predict(&co, cw, &mut out);
    left_predict(&cg, cw, &mut out);
    out
}

fn left_predict(plane: &[u8], w: usize, out: &mut Vec<u8>) {
    for row in plane.chunks_exact(w) {
        let mut prev = 0u8;
        for &v in row {
            out.push(v.wrapping_sub(prev));
            prev = v;
        }
    }
}

fn left_unpredict(data: &[u8], w: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for row in data.chunks_exact(w) {
        let mut prev = 0u8;
        for &d in row {
            prev = prev.wrapping_add(d);
            out.push(prev);
        }
    }
    out
}

/// Decodes one tile payload into tightly packed RGBA (`w*h*4`).
pub fn decode_tile(payload: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    let (&enc, data) = payload.split_first()?;
    let mut rgba = vec![255u8; w * h * 4];
    match enc {
        ENC_RGB => {
            let rgb = lz4_flex::block::decompress(data, w * h * 3).ok()?;
            for (dst, src) in rgba.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
                dst[..3].copy_from_slice(src);
            }
        }
        ENC_YCOCG => {
            let (cw, ch) = chroma_dims(w, h);
            let planes = lz4_flex::block::decompress(data, w * h + 2 * cw * ch).ok()?;
            let y = left_unpredict(&planes[..w * h], w);
            let co = left_unpredict(&planes[w * h..w * h + cw * ch], cw);
            let cg = left_unpredict(&planes[w * h + cw * ch..], cw);
            for py in 0..h {
                for px in 0..w {
                    let c = (py / 2) * cw + px / 2;
                    let yy = y[py * w + px] as i32;
                    let co = (co[c] as i32 - 128) * 2;
                    let cg = (cg[c] as i32 - 128) * 4;
                    // Inverse of Y=(R+2G+B)/4, Co=R-B, Cg=2G-R-B.
                    let g = yy + cg / 4;
                    let t = yy - cg / 4;
                    let r = t + co / 2;
                    let b = t - co / 2;
                    let o = (py * w + px) * 4;
                    rgba[o] = r.clamp(0, 255) as u8;
                    rgba[o + 1] = g.clamp(0, 255) as u8;
                    rgba[o + 2] = b.clamp(0, 255) as u8;
                }
            }
        }
        _ => return None,
    }
    Some(rgba)
}

pub struct TileRef<'a> {
    pub tx: usize,
    pub ty: usize,
    pub data: &'a [u8],
}

pub struct FrameHeader {
    pub width: usize,
    pub height: usize,
    pub tile: usize,
    pub captured_us: u64,
    pub cursor: Cursor,
}

pub struct H264<'a> {
    pub width: usize,
    pub height: usize,
    pub captured_us: u64,
    pub encode_us: u32,
    pub key: bool,
    pub data: &'a [u8],
}

pub enum Message<'a> {
    Frame(FrameHeader, Vec<TileRef<'a>>),
    Cursor(Cursor),
    H264(H264<'a>),
}

#[cfg(windows)]
pub fn encode_h264(f: &super::win::encoder::EncodedFrame) -> Vec<u8> {
    let mut out = Vec::with_capacity(f.data.len() + 32);
    out.push(MSG_H264);
    out.extend_from_slice(&f.width.to_le_bytes());
    out.extend_from_slice(&f.height.to_le_bytes());
    out.extend_from_slice(&f.captured_us.to_le_bytes());
    out.extend_from_slice(&f.encode_us.to_le_bytes());
    out.push(f.key as u8);
    out.extend_from_slice(&f.data);
    out
}

struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.b.len() < n {
            return None;
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn cursor(&mut self) -> Option<Cursor> {
        Some(Cursor { x: self.i32()?, y: self.i32()?, visible: self.u8()? != 0 })
    }
}

pub fn parse(bytes: &[u8]) -> Option<Message<'_>> {
    let mut r = Reader { b: bytes };
    match r.u8()? {
        MSG_FRAME => {
            let (width, height, tile) = (r.u32()? as usize, r.u32()? as usize, r.u16()? as usize);
            let _frame_number = r.u64()?;
            let header = FrameHeader { width, height, tile, captured_us: r.u64()?, cursor: r.cursor()? };
            if header.tile != TILE || header.width == 0 || header.height == 0 {
                return None;
            }
            if header.width > 16384 || header.height > 16384 {
                return None;
            }
            let (cols, rows) = grid(header.width, header.height);
            let n = r.u32()? as usize;
            let mut tiles = Vec::with_capacity(n.min(cols * rows));
            for _ in 0..n {
                let tx = r.u16()? as usize;
                let ty = r.u16()? as usize;
                let len = r.u32()? as usize;
                let data = r.take(len)?;
                if tx < cols && ty < rows {
                    tiles.push(TileRef { tx, ty, data });
                }
            }
            Some(Message::Frame(header, tiles))
        }
        MSG_CURSOR => Some(Message::Cursor(r.cursor()?)),
        MSG_H264 => {
            let (width, height) = (r.u32()? as usize, r.u32()? as usize);
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                return None;
            }
            Some(Message::H264(H264 {
                width,
                height,
                captured_us: r.u64()?,
                encode_us: r.u32()?,
                key: r.u8()? != 0,
                data: r.b,
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, mut f: impl FnMut(usize, usize) -> [u8; 3]) -> Frame {
        let mut rgba = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let [r, g, b] = f(x, y);
                rgba.extend_from_slice(&[r, g, b, 255]);
            }
        }
        Frame { width: w, height: h, rgba, captured_us: 0 }
    }

    fn apply(msg: &[u8], canvas: &mut Vec<u8>, w: usize, h: usize) {
        let Some(Message::Frame(hdr, tiles)) = parse(msg) else { panic!("bad message") };
        assert_eq!((hdr.width, hdr.height), (w, h));
        for t in tiles {
            let r = tile_rect(w, h, t.tx, t.ty);
            let px = decode_tile(t.data, r.w, r.h).expect("tile decodes");
            for row in 0..r.h {
                let dst = ((r.y + row) * w + r.x) * 4;
                canvas[dst..dst + r.w * 4].copy_from_slice(&px[row * r.w * 4..(row + 1) * r.w * 4]);
            }
        }
    }

    #[test]
    fn lossless_ui_roundtrip_and_delta() {
        let (w, h) = (200, 130); // non-multiple of tile size on purpose
        let ui = |x: usize, y: usize| {
            if (x / 7 + y / 11) % 2 == 0 { [30, 30, 40] } else { [230, 230, 240] }
        };
        let f1 = AnalyzedFrame::new(frame(w, h, ui), 1);
        let mut known = Vec::new();
        let msg = f1.encode_delta(&mut known, Cursor::default()).unwrap();
        let mut canvas = vec![0u8; w * h * 4];
        apply(&msg, &mut canvas, w, h);
        assert_eq!(canvas, f1.frame.rgba);

        // Unchanged frame → nothing to send.
        let same = AnalyzedFrame::new(frame(w, h, ui), 2);
        assert!(same.encode_delta(&mut known, Cursor::default()).is_none());

        // One pixel changes → exactly one tile goes out.
        let mut f3 = frame(w, h, ui);
        f3.rgba[(70 * w + 150) * 4] = 1;
        let f3 = AnalyzedFrame::new(f3, 3);
        let msg = f3.encode_delta(&mut known, Cursor::default()).unwrap();
        let Some(Message::Frame(_, tiles)) = parse(&msg) else { panic!() };
        assert_eq!(tiles.len(), 1);
        assert_eq!((tiles[0].tx, tiles[0].ty), (2, 1));
        apply(&msg, &mut canvas, w, h);
        assert_eq!(canvas, f3.frame.rgba);
    }

    #[test]
    fn photographic_tiles_use_lossy_path_with_small_error() {
        let (w, h) = (128, 128);
        let mut seed = 12345u32;
        let photo = frame(w, h, |x, y| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let n = (seed >> 24) as i32 % 24;
            let base = |v: usize| ((v as i32 * 2 + n) % 256) as u8;
            [base(x), base(y), base(x + y)]
        });
        let f = AnalyzedFrame::new(photo, 1);
        let msg = f.encode_delta(&mut Vec::new(), Cursor::default()).unwrap();
        let Some(Message::Frame(_, tiles)) = parse(&msg) else { panic!() };
        assert!(tiles.iter().any(|t| t.data[0] == ENC_YCOCG));
        let mut canvas = vec![0u8; w * h * 4];
        apply(&msg, &mut canvas, w, h);
        let err: f64 = canvas
            .iter()
            .zip(&f.frame.rgba)
            .map(|(a, b)| (*a as f64 - *b as f64).abs())
            .sum::<f64>()
            / canvas.len() as f64;
        assert!(err < 12.0, "mean abs error {err}");
    }

    #[test]
    fn cursor_message() {
        let c = Cursor { x: -5, y: 900, visible: true };
        let Some(Message::Cursor(got)) = parse(&encode_cursor(c)) else { panic!() };
        assert_eq!(got, c);
    }
}

//! Peer-to-peer wire formats.
//!
//! Everything between peers runs over QUIC on one UDP port (see [`crate::tls`]):
//! - Discovery: postcard-encoded [`Beacon`] broadcast on the LAN (plain UDP,
//!   only a hint: the QUIC handshake proves who is who).
//! - Streams: `u32 LE length` + payload frames. The first frame each way is a
//!   [`Hello`], then control streams carry [`Ctrl`] messages and screen
//!   streams carry `screen::codec` frames.
//! - QUIC datagrams: hand-rolled fixed layouts (see [`Datagram`]) to keep the
//!   audio path allocation-free and trivially cheap to parse.

use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

pub type PeerId = u64;

pub const MAGIC: [u8; 4] = *b"DSTU";
/// Bumped on incompatible changes; also part of the QUIC ALPN.
pub const VERSION: u16 = 3;
pub const DISCOVERY_PORT: u16 = 47800;
pub const DEFAULT_PORT: u16 = 47802;

/// Audio is 48 kHz mono, sent in 2.5 ms blocks.
pub const SAMPLE_RATE: u32 = 48_000;
pub const BLOCK: usize = 120;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Beacon {
    pub magic: [u8; 4],
    pub version: u16,
    pub id: PeerId,
    pub name: String,
    pub port: u16,
    /// [`crate::identity::Group::tag`]: only peers in the same group dial.
    pub group: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamKind {
    Control,
    Screen,
}

/// First frame on every stream. Who the sender is comes from its TLS
/// certificate, not from here.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
    pub kind: StreamKind,
    /// [`crate::identity::Group::proof`] for this connection.
    pub proof: [u8; 32],
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub width: u32,
    pub height: u32,
    pub monitor: String,
}

pub type ChannelId = u64;

/// A voice channel. The broker keeps them per group and lists one while its
/// creator is online. Membership is each peer's [`PeerStatus::channel`];
/// audio stays peer-to-peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub name: String,
}

/// Where a peer is. `None` in [`PeerStatus::channel`] is the lobby.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChannelRef {
    pub owner: PeerId,
    pub id: ChannelId,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PeerStatus {
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    pub sharing: Option<ShareInfo>,
    pub channel: Option<ChannelRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Ctrl {
    Status(PeerStatus),
    Bye,
}

pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    postcard::to_stdvec(value).expect("postcard encoding of owned data cannot fail")
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Option<T> {
    postcard::from_bytes(bytes).ok()
}

fn framed(payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

/// Writes one length-prefixed frame with a single `write_all` so small
/// control messages leave in one packet.
pub fn write_frame(w: &mut impl Write, payload: &[u8]) -> io::Result<()> {
    w.write_all(&framed(payload))
}

/// [`write_frame`] of an encoded message on a QUIC stream.
pub async fn send_msg<T: Serialize>(s: &mut quinn::SendStream, msg: &T) -> io::Result<()> {
    s.write_all(&framed(&encode(msg))).await.map_err(io::Error::other)
}

/// Reads one frame from a QUIC stream and decodes it; `None` on a closed
/// stream, an oversized frame or garbage.
pub async fn recv_msg<T: for<'a> Deserialize<'a>>(r: &mut quinn::RecvStream, max_len: usize) -> Option<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.ok()?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max_len {
        return None;
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await.ok()?;
    decode(&buf)
}

pub fn read_frame(r: &mut impl Read, buf: &mut Vec<u8>, max_len: usize) -> io::Result<()> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > max_len {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    buf.resize(len, 0);
    r.read_exact(buf)
}

// ---------------------------------------------------------------------------
// QUIC media datagrams
//
//   common header: b'D' | kind u8 | sender u64
//   (`sender` is informational: receivers use the connection's identity)
//   Audio: seq u32 | capture_us u64 | flags u8 | count u16 | i16 samples
//   Stream: same as Audio, interleaved stereo samples
//   Ping:  t0 u64
//   Pong:  t0 u64 | t_remote u64
// ---------------------------------------------------------------------------

const DGRAM_MAGIC: u8 = b'D';
const KIND_AUDIO: u8 = 1;
const KIND_PING: u8 = 2;
const KIND_PONG: u8 = 3;
const KIND_STREAM: u8 = 4;
const HEADER: usize = 10;

pub const AUDIO_FLAG_VOICE: u8 = 1;

pub const MAX_DATAGRAM: usize = HEADER + 15 + BLOCK * 4 + 64;

#[derive(Debug)]
pub enum Datagram<'a> {
    Audio {
        sender: PeerId,
        seq: u32,
        flags: u8,
        samples: &'a [u8],
    },
    /// Screen-share audio, interleaved stereo.
    Stream {
        sender: PeerId,
        seq: u32,
        samples: &'a [u8],
    },
    Ping {
        t0: u64,
    },
    Pong {
        sender: PeerId,
        t0: u64,
        t_remote: u64,
    },
}

impl<'a> Datagram<'a> {
    pub fn parse(b: &'a [u8]) -> Option<Self> {
        if b.len() < HEADER || b[0] != DGRAM_MAGIC {
            return None;
        }
        let sender = u64::from_le_bytes(b[2..10].try_into().ok()?);
        let body = &b[HEADER..];
        match b[1] {
            KIND_AUDIO if body.len() >= 15 => {
                let seq = u32::from_le_bytes(body[0..4].try_into().ok()?);
                let flags = body[12];
                let count = u16::from_le_bytes(body[13..15].try_into().ok()?) as usize;
                let samples = body.get(15..15 + count * 2)?;
                Some(Datagram::Audio { sender, seq, flags, samples })
            }
            KIND_STREAM if body.len() >= 15 => {
                let seq = u32::from_le_bytes(body[0..4].try_into().ok()?);
                let count = u16::from_le_bytes(body[13..15].try_into().ok()?) as usize;
                let samples = body.get(15..15 + count * 2)?;
                Some(Datagram::Stream { sender, seq, samples })
            }
            KIND_PING if body.len() >= 8 => Some(Datagram::Ping {
                t0: u64::from_le_bytes(body[0..8].try_into().ok()?),
            }),
            KIND_PONG if body.len() >= 16 => Some(Datagram::Pong {
                sender,
                t0: u64::from_le_bytes(body[0..8].try_into().ok()?),
                t_remote: u64::from_le_bytes(body[8..16].try_into().ok()?),
            }),
            _ => None,
        }
    }
}

fn header(out: &mut Vec<u8>, kind: u8, sender: PeerId) {
    out.clear();
    out.push(DGRAM_MAGIC);
    out.push(kind);
    out.extend_from_slice(&sender.to_le_bytes());
}

pub fn write_audio(
    out: &mut Vec<u8>,
    sender: PeerId,
    seq: u32,
    capture_us: u64,
    flags: u8,
    samples: &[f32],
) {
    header(out, KIND_AUDIO, sender);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&capture_us.to_le_bytes());
    out.push(flags);
    out.extend_from_slice(&(samples.len() as u16).to_le_bytes());
    for &s in samples {
        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// Same layout as audio, different kind; `samples` is interleaved stereo.
pub fn write_stream(out: &mut Vec<u8>, sender: PeerId, seq: u32, samples: &[f32]) {
    write_audio(out, sender, seq, 0, 0, samples);
    out[1] = KIND_STREAM;
}

pub fn write_ping(out: &mut Vec<u8>, sender: PeerId, t0: u64) {
    header(out, KIND_PING, sender);
    out.extend_from_slice(&t0.to_le_bytes());
}

pub fn write_pong(out: &mut Vec<u8>, sender: PeerId, t0: u64, t_remote: u64) {
    header(out, KIND_PONG, sender);
    out.extend_from_slice(&t0.to_le_bytes());
    out.extend_from_slice(&t_remote.to_le_bytes());
}

pub fn decode_pcm(bytes: &[u8], out: &mut [f32]) -> usize {
    let n = (bytes.len() / 2).min(out.len());
    for (o, c) in out.iter_mut().zip(bytes.chunks_exact(2)).take(n) {
        *o = i16::from_le_bytes([c[0], c[1]]) as f32 / i16::MAX as f32;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_roundtrip() {
        let samples: Vec<f32> = (0..BLOCK).map(|i| (i as f32 / BLOCK as f32) - 0.5).collect();
        let mut buf = Vec::new();
        write_audio(&mut buf, 42, 7, 1234, AUDIO_FLAG_VOICE, &samples);
        assert!(buf.len() <= MAX_DATAGRAM);
        let Some(Datagram::Audio { sender, seq, flags, samples: pcm }) = Datagram::parse(&buf)
        else {
            panic!("not audio")
        };
        assert_eq!((sender, seq, flags), (42, 7, AUDIO_FLAG_VOICE));
        let mut out = [0f32; BLOCK];
        assert_eq!(decode_pcm(pcm, &mut out), BLOCK);
        for (a, b) in out.iter().zip(&samples) {
            assert!((a - b).abs() < 1e-4);
        }
    }

    #[test]
    fn rejects_garbage() {
        assert!(Datagram::parse(b"hello world").is_none());
        assert!(Datagram::parse(&[b'D', 1, 0, 0, 0, 0, 0, 0, 0, 0, 1]).is_none());
    }
}

//! Diagnostic recorder: the last 10 s of every stage of the voice path, saved
//! on request as 32-bit float WAVs plus a per-block CSV, to find where a click
//! or crackle starts (our mic, what we send, each peer as received, what we
//! play) and what the buffers were doing at that moment.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use crate::protocol::{BLOCK, PeerId, SAMPLE_RATE};

pub const SECONDS: usize = 10;
const TICKS: usize = SECONDS * SAMPLE_RATE as usize / BLOCK;
const LEN: usize = TICKS * BLOCK;
pub const MAX_PEERS: usize = 3;

type Block = [f32; BLOCK];

#[derive(Debug, Clone, Copy, Default)]
pub struct PeerTick {
    pub id: PeerId,
    /// 0 unknown, 1 us, 2 them, 3 both.
    pub talker: u8,
    pub voice: bool,
    pub target: u16,
    pub lost: u64,
    pub underruns: u64,
    pub late: u64,
}

/// Per-block state worth correlating with the audio.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tick {
    pub t_us: u64,
    pub speaking: bool,
    pub roommate: bool,
    pub gate: f32,
    /// Playback ring fill (frames) and the slack the controller aims for.
    pub fill: u32,
    pub headroom: f32,
    pub out_underruns: u64,
    pub xruns: u64,
    pub peers: [PeerTick; MAX_PEERS],
}

pub struct Recorder {
    mic_in: Vec<f32>,
    mic_out: Vec<f32>,
    playback: Vec<f32>,
    /// Screen-share audio as received (mono mix of the first streaming peer).
    stream: Vec<f32>,
    peers: Vec<(PeerId, Vec<f32>)>,
    log: Vec<Tick>,
    pos: usize,
    filled: usize,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            mic_in: vec![0.0; LEN],
            mic_out: vec![0.0; LEN],
            playback: vec![0.0; LEN],
            stream: vec![0.0; LEN],
            peers: (0..MAX_PEERS).map(|_| (0, vec![0.0; LEN])).collect(),
            log: vec![Tick::default(); TICKS],
            pos: 0,
            filled: 0,
        }
    }

    /// `peers`: (id, block as received from their jitter buffer).
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &mut self,
        mic_in: &Block,
        mic_out: &Block,
        playback: &Block,
        stream: &Block,
        peers: &[(PeerId, Block)],
        tick: Tick,
    ) {
        let at = self.pos * BLOCK..(self.pos + 1) * BLOCK;
        self.mic_in[at.clone()].copy_from_slice(mic_in);
        self.mic_out[at.clone()].copy_from_slice(mic_out);
        self.playback[at.clone()].copy_from_slice(playback);
        self.stream[at.clone()].copy_from_slice(stream);
        for slot in &mut self.peers {
            slot.1[at.clone()].fill(0.0);
        }
        for (id, blk) in peers {
            let slot = match self.peers.iter().position(|(p, _)| p == id) {
                Some(i) => Some(i),
                None => self.peers.iter().position(|(p, _)| *p == 0),
            };
            if let Some(i) = slot {
                self.peers[i].0 = *id;
                self.peers[i].1[at.clone()].copy_from_slice(blk);
            }
        }
        self.log[self.pos] = tick;
        self.pos = (self.pos + 1) % TICKS;
        self.filled = (self.filled + 1).min(TICKS);
    }

    /// Oldest-first copy of everything recorded so far.
    pub fn snapshot(&self) -> Snapshot {
        let start = if self.filled < TICKS { 0 } else { self.pos };
        let order = |v: &[f32]| -> Vec<f32> {
            let s = start * BLOCK;
            let mut out = Vec::with_capacity(self.filled * BLOCK);
            out.extend_from_slice(&v[s..]);
            out.extend_from_slice(&v[..s]);
            out.truncate(self.filled * BLOCK);
            out
        };
        let mut log = Vec::with_capacity(self.filled);
        log.extend_from_slice(&self.log[start..]);
        log.extend_from_slice(&self.log[..start]);
        log.truncate(self.filled);
        Snapshot {
            mic_in: order(&self.mic_in),
            mic_out: order(&self.mic_out),
            playback: order(&self.playback),
            stream: order(&self.stream),
            peers: self.peers.iter().filter(|(id, _)| *id != 0).map(|(id, v)| (*id, order(v))).collect(),
            log,
        }
    }
}

pub struct Snapshot {
    mic_in: Vec<f32>,
    mic_out: Vec<f32>,
    playback: Vec<f32>,
    stream: Vec<f32>,
    peers: Vec<(PeerId, Vec<f32>)>,
    log: Vec<Tick>,
}

impl Snapshot {
    pub fn save(&self, dir: &Path, info: &str) -> io::Result<()> {
        fs::create_dir_all(dir)?;
        fs::write(dir.join("info.txt"), info)?;
        write_wav(&dir.join("1-mic-in.wav"), &self.mic_in)?;
        write_wav(&dir.join("2-mic-sent.wav"), &self.mic_out)?;
        for (id, v) in &self.peers {
            write_wav(&dir.join(format!("3-peer-{id:016x}.wav")), v)?;
        }
        write_wav(&dir.join("4-playback.wav"), &self.playback)?;
        if self.stream.iter().any(|&s| s != 0.0) {
            write_wav(&dir.join("3-stream-audio.wav"), &self.stream)?;
        }

        let mut csv = io::BufWriter::new(fs::File::create(dir.join("log.csv"))?);
        write!(csv, "block,t_ms,speaking,roommate,gate,fill,headroom,out_underruns,xruns")?;
        for i in 0..MAX_PEERS {
            write!(csv, ",p{i}_id,p{i}_talker,p{i}_voice,p{i}_target,p{i}_lost,p{i}_underruns,p{i}_late")?;
        }
        writeln!(csv)?;
        let t0 = self.log.first().map_or(0, |t| t.t_us);
        for (n, t) in self.log.iter().enumerate() {
            write!(
                csv,
                "{n},{:.1},{},{},{:.3},{},{:.0},{},{}",
                (t.t_us.saturating_sub(t0)) as f64 / 1000.0,
                t.speaking as u8,
                t.roommate as u8,
                t.gate,
                t.fill,
                t.headroom,
                t.out_underruns,
                t.xruns
            )?;
            for p in &t.peers {
                write!(
                    csv,
                    ",{:016x},{},{},{},{},{},{}",
                    p.id, p.talker, p.voice as u8, p.target, p.lost, p.underruns, p.late
                )?;
            }
            writeln!(csv)?;
        }
        csv.flush()
    }
}

/// Mono 48 kHz IEEE-float WAV.
fn write_wav(path: &Path, samples: &[f32]) -> io::Result<()> {
    let data = samples.len() as u32 * 4;
    let mut out = Vec::with_capacity(44 + data as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    out.extend_from_slice(&(SAMPLE_RATE * 4).to_le_bytes());
    out.extend_from_slice(&4u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    fs::write(path, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_oldest_first_after_wrapping() {
        let mut r = Recorder::new();
        for n in 0..TICKS + 5 {
            let b = [n as f32; BLOCK];
            r.record(&b, &b, &b, &b, &[(7, b)], Tick { t_us: n as u64, ..Default::default() });
        }
        let s = r.snapshot();
        assert_eq!(s.mic_in.len(), LEN);
        assert_eq!(s.mic_in[0], 5.0);
        assert_eq!(*s.mic_in.last().unwrap(), (TICKS + 4) as f32);
        assert_eq!(s.log[0].t_us, 5);
        assert_eq!(s.peers[0].0, 7);
        assert_eq!(s.peers[0].1[0], 5.0);
    }
}

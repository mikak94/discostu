//! Notification sounds (someone entered or left our channel), mixed into
//! the playback on the DSP thread.

use super::resample::Resampler;
use crate::protocol::SAMPLE_RATE;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cue {
    Enter = 1,
    Leave = 2,
}

impl Cue {
    pub(super) fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Enter),
            2 => Some(Self::Leave),
            _ => None,
        }
    }

    fn index(self) -> usize {
        self as usize - 1
    }
}

/// Cues playing at once; more are dropped.
const VOICES: usize = 4;

pub struct CuePlayer {
    /// Interleaved stereo at [`SAMPLE_RATE`], indexed by [`Cue::index`].
    sounds: [Vec<f32>; 2],
    /// (sound, position in samples) of each cue still playing.
    playing: Vec<(usize, usize)>,
}

impl CuePlayer {
    pub fn new() -> Self {
        Self {
            sounds: [
                decode_wav(include_bytes!("../../assets/sounds/enters.wav")),
                decode_wav(include_bytes!("../../assets/sounds/leaves.wav")),
            ],
            playing: Vec::with_capacity(VOICES),
        }
    }

    /// Starts `cue` over whatever is playing. The same cue started within the
    /// last second (several people arriving at once, e.g. on launch) plays
    /// once instead of piling up.
    pub fn play(&mut self, cue: Cue) {
        let i = cue.index();
        let recent = SAMPLE_RATE as usize * 2;
        if self.playing.len() >= VOICES || self.playing.iter().any(|&(s, pos)| s == i && pos < recent) {
            return;
        }
        self.playing.push((i, 0));
    }

    /// Adds the playing cues to an interleaved stereo block.
    pub fn mix(&mut self, out: &mut [f32]) {
        let sounds = &self.sounds;
        self.playing.retain_mut(|(i, pos)| {
            let s = &sounds[*i];
            let n = out.len().min(s.len() - *pos);
            for (o, x) in out.iter_mut().zip(&s[*pos..*pos + n]) {
                *o += x;
            }
            *pos += n;
            *pos < s.len()
        });
    }
}

/// 16-bit PCM WAV (mono or stereo, any rate) → interleaved stereo at
/// [`SAMPLE_RATE`]. Anything else decodes to silence.
fn decode_wav(wav: &[u8]) -> Vec<f32> {
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    let (mut channels, mut rate, mut bits, mut data) = (0usize, 0u32, 0u16, &[][..]);
    let mut p = 12;
    while p + 8 <= wav.len() {
        let len = u32_at(wav, p + 4) as usize;
        let body = &wav[p + 8..(p + 8 + len).min(wav.len())];
        match &wav[p..p + 4] {
            b"fmt " if body.len() >= 16 => {
                channels = u16_at(body, 2) as usize;
                rate = u32_at(body, 4);
                bits = u16_at(body, 14);
            }
            b"data" => data = body,
            _ => {}
        }
        p += 8 + len + (len & 1);
    }
    if bits != 16 || !(1..=2).contains(&channels) || rate == 0 {
        return Vec::new();
    }
    let sample = |f: &[u8], c: usize| i16::from_le_bytes([f[2 * c], f[2 * c + 1]]) as f32 / 32768.0;
    let stereo: Vec<f32> = data
        .chunks_exact(2 * channels)
        .flat_map(|f| [sample(f, 0), sample(f, channels - 1)])
        .collect();
    let mut out = Vec::with_capacity(stereo.len() * SAMPLE_RATE as usize / rate as usize + 8);
    Resampler::with_channels(rate, SAMPLE_RATE, 2).process_interleaved(&stereo, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sounds_decode() {
        let player = CuePlayer::new();
        for s in &player.sounds {
            let seconds = s.len() as f32 / 2.0 / SAMPLE_RATE as f32;
            assert!((1.0..10.0).contains(&seconds), "{seconds} s");
            let peak = s.iter().fold(0f32, |m, x| m.max(x.abs()));
            assert!((0.05..=1.0).contains(&peak), "peak {peak}");
        }
    }

    #[test]
    fn same_cue_at_once_plays_once() {
        let mut player = CuePlayer::new();
        player.play(Cue::Enter);
        player.play(Cue::Enter);
        player.play(Cue::Leave);
        assert_eq!(player.playing.len(), 2);
        let mut block = [0f32; 240];
        while !player.playing.is_empty() {
            player.mix(&mut block);
        }
    }
}

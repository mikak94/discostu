//! Voice analysis: mel-band spectra, noise floor tracking, VAD, and the
//! long-term voice profile.
//!
//! A [`VoiceProfile`] is the average spectral *shape* of one person's voice
//! (24 mel bands, level-normalised) learned from every frame the VAD marks as
//! confident speech. It sharpens over minutes of talking and is persisted, so
//! it keeps improving across sessions. It is used to:
//! - gate the mic on "sounds like me" rather than raw loudness, and
//! - tell our voice from a roommate's in our mic (same-room separation).
//!
//! It never changes the sound: it's a yes/no input to those decisions. Users
//! can see it, reshape it by hand and exclude bands (Settings).

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{RealFftPlanner, RealToComplex};
use serde::{Deserialize, Serialize};

use crate::protocol::SAMPLE_RATE;

pub const BANDS: usize = 24;
const FFT: usize = 512;
const F_LO: f32 = 90.0;
const F_HI: f32 = 7600.0;

pub type Bands = [f32; BANDS];

/// Sliding 512-sample analysis producing log mel-band energies (dB).
pub struct Analyzer {
    ring: Vec<f32>,
    pos: usize,
    window: Vec<f32>,
    fft: Arc<dyn RealToComplex<f32>>,
    input: Vec<f32>,
    spec: Vec<Complex32>,
    filters: Vec<(usize, Vec<f32>)>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    pub fn new() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT);
        let window = (0..FFT)
            .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / FFT as f32).cos())
            .collect();
        Self {
            ring: vec![0.0; FFT],
            pos: 0,
            window,
            input: fft.make_input_vec(),
            spec: fft.make_output_vec(),
            fft,
            filters: mel_filters(),
        }
    }

    pub fn push(&mut self, block: &[f32]) {
        for &s in block {
            self.ring[self.pos] = s;
            self.pos = (self.pos + 1) % FFT;
        }
    }

    pub fn analyze(&mut self) -> Bands {
        for i in 0..FFT {
            self.input[i] = self.ring[(self.pos + i) % FFT] * self.window[i];
        }
        let _ = self.fft.process(&mut self.input, &mut self.spec);
        let mut out = [0f32; BANDS];
        for (b, (start, weights)) in self.filters.iter().enumerate() {
            let p: f32 = weights
                .iter()
                .enumerate()
                .map(|(k, w)| w * self.spec[start + k].norm_sqr())
                .sum();
            out[b] = 10.0 * (p / FFT as f32 + 1e-10).log10();
        }
        out
    }
}

fn mel(f: f32) -> f32 {
    2595.0 * (1.0 + f / 700.0).log10()
}

fn inv_mel(m: f32) -> f32 {
    700.0 * (10f32.powf(m / 2595.0) - 1.0)
}

fn mel_filters() -> Vec<(usize, Vec<f32>)> {
    let bin_hz = SAMPLE_RATE as f32 / FFT as f32;
    let (lo, hi) = (mel(F_LO), mel(F_HI));
    let edges: Vec<f32> = (0..BANDS + 2)
        .map(|i| inv_mel(lo + (hi - lo) * i as f32 / (BANDS + 1) as f32) / bin_hz)
        .collect();
    (0..BANDS)
        .map(|b| {
            let (l, c, r) = (edges[b], edges[b + 1], edges[b + 2]);
            let start = l.floor() as usize;
            let end = (r.ceil() as usize).min(FFT / 2);
            let weights = (start..=end)
                .map(|k| {
                    let k = k as f32;
                    if k < c { (k - l) / (c - l) } else { (r - k) / (r - c) }.max(0.0)
                })
                .collect();
            (start, weights)
        })
        .collect()
}

/// Per-band minimum-statistics noise floor.
pub struct NoiseFloor {
    floor: Bands,
    primed: bool,
}

impl Default for NoiseFloor {
    fn default() -> Self {
        Self { floor: [-100.0; BANDS], primed: false }
    }
}

impl NoiseFloor {
    /// Updates with one frame and returns the mean positive SNR in dB.
    pub fn update(&mut self, bands: &Bands) -> f32 {
        if !self.primed {
            self.floor = *bands;
            self.primed = true;
        }
        let mut snr = 0.0;
        for (f, &b) in self.floor.iter_mut().zip(bands) {
            if b < *f {
                *f += 0.2 * (b - *f); // fall quickly
            } else {
                *f += 0.0015 * (b - *f).min(20.0); // rise slowly (~seconds)
            }
            snr += (b - *f).max(0.0);
        }
        snr / BANDS as f32
    }
}

/// Energy/SNR voice activity detector with hangover.
pub struct Vad {
    hang: u32,
    pub speaking: bool,
    pub snr: f32,
}

impl Default for Vad {
    fn default() -> Self {
        Self { hang: 0, speaking: false, snr: 0.0 }
    }
}

impl Vad {
    /// `blocks_hang`: blocks to stay "speaking" after the last voiced frame.
    pub fn update(&mut self, snr: f32, level_db: f32, blocks_hang: u32) -> bool {
        self.snr = snr;
        let on = snr > if self.speaking { 4.0 } else { 7.0 } && level_db > -62.0;
        if on {
            self.hang = blocks_hang;
            self.speaking = true;
        } else if self.hang > 0 {
            self.hang -= 1;
        } else {
            self.speaking = false;
        }
        self.speaking
    }
}

/// Seconds of confident speech after which the profile counts as trained.
pub const TRAINED_SECONDS: f32 = 90.0;
/// Frames per second fed to the profile (one per 2.5 ms block).
const FRAMES_PER_SECOND: f32 = SAMPLE_RATE as f32 / crate::protocol::BLOCK as f32;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceProfile {
    /// Mean level-normalised band shape (dB).
    pub mean: Bands,
    /// Per-band variance of the shape (dB²).
    pub var: Bands,
    pub frames: u64,
    /// Bands the user excluded (a fan's hum, a whine): they count neither
    /// in the shape nor in the comparison.
    #[serde(default)]
    pub ignored: [bool; BANDS],
}

impl Default for VoiceProfile {
    fn default() -> Self {
        Self { mean: [0.0; BANDS], var: [36.0; BANDS], frames: 0, ignored: [false; BANDS] }
    }
}

/// How far the user can drag a band, in dB from the voice's average.
pub const EDIT_RANGE_DB: f32 = 30.0;

/// Centre frequency of each band (Hz), for labelling.
pub fn band_centers() -> Bands {
    let (lo, hi) = (mel(F_LO), mel(F_HI));
    std::array::from_fn(|b| inv_mel(lo + (hi - lo) * (b + 1) as f32 / (BANDS + 1) as f32))
}

/// Level-normalised shape: each band relative to the average of the bands
/// that count (ignored ones come out as 0).
pub fn shape(bands: &Bands, ignored: &[bool; BANDS]) -> Bands {
    let used = ignored.iter().filter(|i| !**i).count().max(1);
    let mean = bands.iter().zip(ignored).filter(|(_, i)| !**i).map(|(b, _)| b).sum::<f32>() / used as f32;
    std::array::from_fn(|i| if ignored[i] { 0.0 } else { bands[i] - mean })
}

impl VoiceProfile {
    pub fn seconds(&self) -> f32 {
        self.frames as f32 / FRAMES_PER_SECOND
    }

    pub fn progress(&self) -> f32 {
        (self.seconds() / TRAINED_SECONDS).min(1.0)
    }

    /// Enough data to make decisions with (~8 s of speech).
    pub fn usable(&self) -> bool {
        self.seconds() > 8.0
    }

    pub fn learn(&mut self, bands: &Bands) {
        let s = shape(bands, &self.ignored);
        self.frames += 1;
        // Running mean at first, then a slow EMA (~10 min of speech) so the
        // profile follows mic/room changes without forgetting the voice.
        let a = (1.0 / self.frames as f32).max(1.0 / (600.0 * FRAMES_PER_SECOND));
        for i in 0..BANDS {
            let d = s[i] - self.mean[i];
            self.mean[i] += a * d;
            self.var[i] += a * (d * d - self.var[i]);
            self.var[i] = self.var[i].clamp(1.0, 400.0);
        }
    }

    /// Sets one band of the learned shape by hand.
    pub fn set_band(&mut self, band: usize, db: f32) {
        if band < BANDS {
            self.mean[band] = db.clamp(-EDIT_RANGE_DB, EDIT_RANGE_DB);
        }
    }

    pub fn toggle_ignored(&mut self, band: usize) {
        if band < BANDS {
            self.ignored[band] = !self.ignored[band];
        }
    }

    /// Variance-weighted cosine similarity of spectral shape, in -1..1.
    pub fn similarity(&self, bands: &Bands) -> f32 {
        let s = shape(bands, &self.ignored);
        let (mut num, mut na, mut nb) = (0.0, 0.0, 0.0);
        for i in (0..BANDS).filter(|i| !self.ignored[*i]) {
            let w = 1.0 / self.var[i].sqrt();
            let (a, b) = (s[i] * w, self.mean[i] * w);
            num += a * b;
            na += a * a;
            nb += b * b;
        }
        num / ((na * nb).sqrt() + 1e-9)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::BLOCK;

    fn tone_mix(freqs: &[f32], n: usize, offset: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                let t = (i + offset) as f32 / SAMPLE_RATE as f32;
                freqs.iter().map(|f| (2.0 * std::f32::consts::PI * f * t).sin()).sum::<f32>() * 0.1
            })
            .collect()
    }

    #[test]
    fn profile_distinguishes_spectra() {
        let mut an = Analyzer::new();
        let mut me = VoiceProfile::default();
        let low = [180.0, 360.0, 540.0, 900.0];
        let high = [1800.0, 2600.0, 3400.0, 5000.0];
        for b in 0..4000 {
            an.push(&tone_mix(&low, BLOCK, b * BLOCK));
            me.learn(&an.analyze());
        }
        assert!(me.usable());
        an.push(&tone_mix(&low, 512, 0));
        let same = me.similarity(&an.analyze());
        an.push(&tone_mix(&high, 512, 0));
        let other = me.similarity(&an.analyze());
        assert!(same > 0.9, "same {same}");
        assert!(other < same - 0.5, "other {other} vs same {same}");
    }

    #[test]
    fn vad_follows_snr() {
        let mut nf = NoiseFloor::default();
        let mut vad = Vad::default();
        let quiet = [-80.0; BANDS];
        for _ in 0..500 {
            let snr = nf.update(&quiet);
            assert!(!vad.update(snr, -80.0, 10));
        }
        let loud = [-30.0; BANDS];
        let snr = nf.update(&loud);
        assert!(vad.update(snr, -30.0, 10));
    }

    #[test]
    fn ignored_bands_do_not_count() {
        let mut p = VoiceProfile::default();
        let voice: Bands = std::array::from_fn(|i| -40.0 - i as f32);
        for _ in 0..4000 {
            p.learn(&voice);
        }
        let near = p.similarity(&voice);
        // A loud hum in band 2 wrecks the match until that band is ignored.
        let mut hum = voice;
        hum[2] += 40.0;
        let with_hum = p.similarity(&hum);
        p.toggle_ignored(2);
        let ignored = p.similarity(&hum);
        assert!(near > 0.99, "{near}");
        assert!(with_hum < 0.8, "{with_hum}");
        assert!(ignored > 0.99, "{ignored}");
    }

    #[test]
    fn edits_are_clamped() {
        let mut p = VoiceProfile::default();
        p.set_band(3, 99.0);
        p.set_band(99, 1.0);
        assert_eq!(p.mean[3], EDIT_RANGE_DB);
    }
}

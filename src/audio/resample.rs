//! Streaming cubic (Catmull-Rom) resampler with a fine rate correction.
//!
//! The correction lets the playback path absorb clock drift between devices
//! by stretching time by a fraction of a percent — inaudible, unlike
//! dropping or duplicating samples.

pub struct Resampler {
    base_step: f64,
    step: f64,
    /// Read position in input frames; negative values index the history.
    pos: f64,
    channels: usize,
    /// Last 3 frames of the previous call, per channel (kernel continuity).
    hist_ch: Vec<[f32; 3]>,
}

impl Resampler {
    pub fn new(from_hz: u32, to_hz: u32) -> Self {
        Self::with_channels(from_hz, to_hz, 1)
    }

    pub fn with_channels(from_hz: u32, to_hz: u32, channels: usize) -> Self {
        let step = from_hz as f64 / to_hz as f64;
        Self { base_step: step, step, pos: 0.0, channels, hist_ch: vec![[0.0; 3]; channels] }
    }

    /// `ratio` > 0 consumes input faster (fewer output samples), e.g. 1e-3 = +0.1 %.
    pub fn set_correction(&mut self, ratio: f64) {
        self.step = self.base_step * (1.0 + ratio);
    }

    fn passthrough(&self) -> bool {
        self.step == 1.0 && self.pos == 0.0
    }

    /// Mono convenience wrapper.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        debug_assert_eq!(self.channels, 1);
        if self.passthrough() {
            out.extend_from_slice(input);
            if input.len() >= 3 {
                self.hist_ch[0].copy_from_slice(&input[input.len() - 3..]);
            }
            return;
        }
        self.process_interleaved(input, out);
    }

    /// Interleaved multi-channel resampling.
    pub fn process_interleaved(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        if self.passthrough() {
            out.extend_from_slice(input);
            for c in 0..ch {
                for k in 0..3 {
                    let f = frames as isize - 3 + k as isize;
                    if f >= 0 {
                        self.hist_ch[c][k] = input[f as usize * ch + c];
                    }
                }
            }
            return;
        }
        // Sample at frame index i (may be -3..-1 → history).
        let at = |hist: &[[f32; 3]], c: usize, i: isize| -> f32 {
            if i < 0 {
                hist[c][(3 + i) as usize]
            } else {
                input[(i as usize).min(frames - 1) * ch + c]
            }
        };
        // Need i+2 < frames for the 4-point kernel around [i, i+1].
        while self.pos + 2.0 < frames as f64 {
            let i = self.pos.floor() as isize;
            let t = (self.pos - i as f64) as f32;
            for c in 0..ch {
                let p0 = at(&self.hist_ch, c, i - 1);
                let p1 = at(&self.hist_ch, c, i);
                let p2 = at(&self.hist_ch, c, i + 1);
                let p3 = at(&self.hist_ch, c, i + 2);
                let a = -0.5 * p0 + 1.5 * p1 - 1.5 * p2 + 0.5 * p3;
                let b = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
                let cc = -0.5 * p0 + 0.5 * p2;
                out.push(((a * t + b) * t + cc) * t + p1);
            }
            self.pos += self.step;
        }
        self.pos -= frames as f64;
        let mut new_hist = self.hist_ch.clone();
        for (c, h) in new_hist.iter_mut().enumerate() {
            for (k, slot) in h.iter_mut().enumerate() {
                *slot = at(&self.hist_ch, c, frames as isize - 3 + k as isize);
            }
        }
        self.hist_ch = new_hist;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_rate_ratio() {
        let mut r = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        let input = vec![0.5; 441];
        for _ in 0..100 {
            r.process(&input, &mut out);
        }
        let expected = 441.0 * 100.0 * 48_000.0 / 44_100.0;
        assert!((out.len() as f64 - expected).abs() < 4.0);
        assert!(out[10..].iter().all(|v| (v - 0.5).abs() < 1e-5));
    }

    #[test]
    fn correction_is_smooth_on_a_sine() {
        let mut r = Resampler::new(48_000, 48_000);
        r.set_correction(0.002);
        let mut out = Vec::new();
        let f = 440.0 / 48_000.0;
        let mut n = 0usize;
        for _ in 0..200 {
            let block: Vec<f32> = (0..120)
                .map(|_| {
                    n += 1;
                    (2.0 * std::f64::consts::PI * f * n as f64).sin() as f32
                })
                .collect();
            r.process(&block, &mut out);
        }
        // Max sample-to-sample step of a 440 Hz sine is ~0.058; any seam
        // would show up as a much larger jump.
        let max_step = out.windows(2).skip(4).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(max_step < 0.07, "step {max_step}");
        let expected = 24_000.0 / 1.002;
        assert!((out.len() as f64 - expected).abs() < 6.0);
    }
}

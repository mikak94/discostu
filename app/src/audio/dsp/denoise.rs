//! Neural noise suppression with no added latency.
//!
//! Every 2.5 ms block, a small recurrent network (trained by `train/`, see
//! `train/dsp.py`, which this mirrors exactly) looks at the last 1024 mic
//! samples, ending with the block itself, and predicts how much of each of 32
//! frequency bands is voice. Those gains become a minimum-phase FIR filter
//! that is applied to the same block straight away, crossfading from the
//! previous block's filter so changes never click. Nothing waits for future
//! samples: a minimum-phase filter's response starts at once, and with all
//! gains at 1 it is an exact passthrough.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use super::dot;
use crate::protocol::BLOCK;

const WIN: usize = 1024;
const NBIN: usize = WIN / 2 + 1;
const FALL: usize = BLOCK;
const RISE: usize = WIN - FALL;
/// Band centres in FFT bins (47 Hz each), ERB-spaced from DC to 20 kHz.
const CENTERS: [usize; NB] = [
    0, 1, 2, 3, 4, 5, 7, 9, 11, 13, 16, 19, 23, 27, 32, 38, 44, 52, 61, 71, 83, 97, 113, 131, 152,
    176, 205, 237, 275, 318, 369, 427,
];
pub const NB: usize = 32;
/// Fine low-frequency bins (pitch harmonics), fed to the network as they are.
const FINE_LO: usize = 1;
const FINE_HI: usize = 49;
const NFEAT: usize = NB + FINE_HI - FINE_LO;
const TAPS: usize = 256;
const MIN_GAIN: f32 = 1e-3;
const EPS: f32 = 1e-9;

static MODEL: &[u8] = include_bytes!("denoise/model.bin");

struct Dense {
    rows: usize,
    cols: usize,
    w: Vec<f32>,
    b: Vec<f32>,
}

impl Dense {
    fn apply(&self, x: &[f32], out: &mut [f32]) {
        for (r, o) in out.iter_mut().enumerate().take(self.rows) {
            *o = dot(&self.w[r * self.cols..(r + 1) * self.cols], x) + self.b[r];
        }
    }
}

/// PyTorch's GRU: gates r, z, n; the reset gate scales the recurrent term
/// after its matrix product.
struct Gru {
    n: usize,
    ih: Dense,
    hh: Dense,
}

impl Gru {
    fn step(&self, x: &[f32], h: &mut [f32], gi: &mut [f32], gh: &mut [f32]) {
        let n = self.n;
        self.ih.apply(x, gi);
        self.hh.apply(h, gh);
        for i in 0..n {
            let r = sigmoid(gi[i] + gh[i]);
            let z = sigmoid(gi[n + i] + gh[n + i]);
            let c = (gi[2 * n + i] + r * gh[2 * n + i]).tanh();
            h[i] = (1.0 - z) * c + z * h[i];
        }
    }
}

fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

struct Model {
    hidden: usize,
    mean: Vec<f32>,
    std: Vec<f32>,
    inp: Dense,
    gru1: Gru,
    gru2: Gru,
    out: Dense,
}

impl Model {
    /// The weights are compiled in, so a mismatch is a build bug: panic.
    fn parse(bytes: &[u8]) -> Self {
        assert_eq!(&bytes[..4], b"DSNS", "denoise model: bad magic");
        let word = |i: usize| u32::from_le_bytes(bytes[4 + 4 * i..8 + 4 * i].try_into().unwrap()) as usize;
        let (version, hidden, nfeat, nb, taps) = (word(0), word(1), word(2), word(3), word(4));
        assert_eq!((version, nfeat, nb, taps), (1, NFEAT, NB, TAPS), "denoise model doesn't match this code");
        let mut pos = 24;
        let mut take = |n: usize| -> Vec<f32> {
            let v = bytes[pos..pos + 4 * n]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                .collect();
            pos += 4 * n;
            v
        };
        let h = hidden;
        let mean = take(NFEAT);
        let std = take(NFEAT);
        let inp = Dense { rows: h, cols: NFEAT, w: take(h * NFEAT), b: take(h) };
        let mut gru = |input: usize| {
            let (w_ih, w_hh) = (take(3 * h * input), take(3 * h * h));
            let (b_ih, b_hh) = (take(3 * h), take(3 * h));
            Gru {
                n: h,
                ih: Dense { rows: 3 * h, cols: input, w: w_ih, b: b_ih },
                hh: Dense { rows: 3 * h, cols: h, w: w_hh, b: b_hh },
            }
        };
        let gru1 = gru(h);
        let gru2 = gru(h);
        let out = Dense { rows: NB, cols: 2 * h, w: take(NB * 2 * h), b: take(NB) };
        assert_eq!(pos, bytes.len(), "denoise model: trailing bytes");
        Self { hidden, mean, std, inp, gru1, gru2, out }
    }
}

pub struct Denoiser {
    model: Model,
    /// Both GRU states, concatenated: the output layer reads them together.
    state: Vec<f32>,
    x: Vec<f32>,
    gi: Vec<f32>,
    gh: Vec<f32>,
    feat: [f32; NFEAT],
    gains: [f32; NB],
    window: Vec<f32>,
    fade: Vec<f32>,
    /// The last WIN mic samples, oldest first.
    hist: Vec<f32>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    buf: Vec<f32>,
    spec: Vec<Complex32>,
    power: Vec<f32>,
    scratch_fwd: Vec<Complex32>,
    scratch_inv: Vec<Complex32>,
    /// Current and next filter, time-reversed for the dot products.
    filter: Vec<f32>,
    next: Vec<f32>,
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    pub fn new() -> Self {
        let model = Model::parse(MODEL);
        let h = model.hidden;
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(WIN);
        let inv = planner.plan_fft_inverse(WIN);
        let window = (0..WIN)
            .map(|n| {
                let pi = std::f64::consts::PI;
                if n < RISE {
                    0.5 - 0.5 * (pi * (n as f64 + 0.5) / RISE as f64).cos()
                } else {
                    0.5 + 0.5 * (pi * ((n - RISE) as f64 + 0.5) / FALL as f64).cos()
                }
            })
            .map(|w| w as f32)
            .collect();
        let quarter = TAPS / 4;
        let fade = (0..TAPS)
            .map(|k| {
                if k < TAPS - quarter {
                    1.0
                } else {
                    let m = (k - (TAPS - quarter)) as f64;
                    (0.5 + 0.5 * (std::f64::consts::PI * (m + 0.5) / quarter as f64).cos()) as f32
                }
            })
            .collect();
        let mut d = Self {
            state: vec![0.0; 2 * h],
            x: vec![0.0; h],
            gi: vec![0.0; 3 * h],
            gh: vec![0.0; 3 * h],
            feat: [0.0; NFEAT],
            gains: [1.0; NB],
            window,
            fade,
            hist: vec![0.0; WIN],
            buf: fwd.make_input_vec(),
            spec: fwd.make_output_vec(),
            power: vec![0.0; NBIN],
            scratch_fwd: fwd.make_scratch_vec(),
            scratch_inv: inv.make_scratch_vec(),
            filter: vec![0.0; TAPS],
            next: vec![0.0; TAPS],
            fwd,
            inv,
            model,
        };
        d.reset();
        d
    }

    /// Forgets everything: silence history, fresh network state, passthrough.
    pub fn reset(&mut self) {
        self.state.fill(0.0);
        self.hist.fill(0.0);
        self.gains = [1.0; NB];
        self.filter.fill(0.0);
        self.filter[TAPS - 1] = 1.0;
    }

    /// Cleans one block in place, with no delay. `amount` scales the
    /// suppression in dB: 1 = as trained, 0.5 = half as deep, 0 = untouched.
    pub fn process(&mut self, block: &mut [f32; BLOCK], amount: f32) {
        self.hist.copy_within(BLOCK.., 0);
        self.hist[WIN - BLOCK..].copy_from_slice(block);

        self.analyze();
        self.predict();
        self.design(amount.clamp(0.0, 1.0));

        // Filter the block, crossfading from last block's filter.
        let start = WIN - BLOCK - (TAPS - 1);
        for (n, s) in block.iter_mut().enumerate() {
            let seg = &self.hist[start + n..start + n + TAPS];
            let old = dot(&self.filter, seg);
            let new = dot(&self.next, seg);
            let r = (n + 1) as f32 / BLOCK as f32;
            *s = (1.0 - r) * old + r * new;
        }
        std::mem::swap(&mut self.filter, &mut self.next);
    }

    /// Power spectrum of the windowed history, then the network's features.
    fn analyze(&mut self) {
        for ((b, h), w) in self.buf.iter_mut().zip(&self.hist).zip(&self.window) {
            *b = h * w;
        }
        let _ = self.fwd.process_with_scratch(&mut self.buf, &mut self.spec, &mut self.scratch_fwd);
        for (p, c) in self.power.iter_mut().zip(&self.spec) {
            *p = c.norm_sqr();
        }
        let mut e = [0f32; NB];
        for i in 0..NB - 1 {
            let (lo, hi) = (CENTERS[i], CENTERS[i + 1]);
            for j in 0..hi - lo {
                let frac = j as f32 / (hi - lo) as f32;
                e[i] += (1.0 - frac) * self.power[lo + j];
                e[i + 1] += frac * self.power[lo + j];
            }
        }
        e[NB - 1] += self.power[CENTERS[NB - 1]..].iter().sum::<f32>();
        for (f, e) in self.feat.iter_mut().zip(e) {
            *f = (e + EPS).log10();
        }
        for (f, p) in self.feat[NB..].iter_mut().zip(&self.power[FINE_LO..FINE_HI]) {
            *f = (p + EPS).log10();
        }
    }

    fn predict(&mut self) {
        let m = &self.model;
        let h = m.hidden;
        for (f, (mean, std)) in self.feat.iter_mut().zip(m.mean.iter().zip(&m.std)) {
            *f = (*f - mean) / std;
        }
        m.inp.apply(&self.feat, &mut self.x);
        for v in &mut self.x {
            *v = v.tanh();
        }
        let (h1, h2) = self.state.split_at_mut(h);
        m.gru1.step(&self.x, h1, &mut self.gi, &mut self.gh);
        m.gru2.step(h1, h2, &mut self.gi, &mut self.gh);
        let mut logits = [0f32; NB];
        m.out.apply(&self.state, &mut logits);
        for (g, l) in self.gains.iter_mut().zip(logits) {
            *g = sigmoid(l);
        }
    }

    /// Band gains -> per-bin magnitude -> minimum-phase FIR (real cepstrum
    /// folding), into `self.next`, time-reversed.
    fn design(&mut self, amount: f32) {
        let mut g = [0f32; NB];
        for (o, &x) in g.iter_mut().zip(&self.gains) {
            *o = x.max(MIN_GAIN).powf(amount);
        }
        // log|H| per bin, interpolated between band centres.
        for c in self.spec.iter_mut() {
            *c = Complex32::new(0.0, 0.0);
        }
        for i in 0..NB - 1 {
            let (lo, hi) = (CENTERS[i], CENTERS[i + 1]);
            for j in 0..hi - lo {
                let frac = j as f32 / (hi - lo) as f32;
                self.spec[lo + j].re = ((1.0 - frac) * g[i] + frac * g[i + 1]).max(MIN_GAIN).ln();
            }
        }
        let last = g[NB - 1].max(MIN_GAIN).ln();
        for c in &mut self.spec[CENTERS[NB - 1]..] {
            c.re = last;
        }
        // Real cepstrum (realfft doesn't normalise: divide by WIN).
        let _ = self.inv.process_with_scratch(&mut self.spec, &mut self.buf, &mut self.scratch_inv);
        let scale = 1.0 / WIN as f32;
        let half = WIN / 2;
        self.buf[0] *= scale;
        for c in &mut self.buf[1..half] {
            *c *= 2.0 * scale;
        }
        self.buf[half] *= scale;
        self.buf[half + 1..].fill(0.0);
        let _ = self.fwd.process_with_scratch(&mut self.buf, &mut self.spec, &mut self.scratch_fwd);
        for c in self.spec.iter_mut() {
            *c = Complex32::from_polar(c.re.exp(), c.im);
        }
        self.spec[0].im = 0.0;
        self.spec[NBIN - 1].im = 0.0;
        let _ = self.inv.process_with_scratch(&mut self.spec, &mut self.buf, &mut self.scratch_inv);
        for k in 0..TAPS {
            self.next[TAPS - 1 - k] = self.buf[k] * scale * self.fade[k];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn floats(b: &[u8]) -> Vec<f32> {
        b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
    }

    /// Same input, same output as the Python reference (`train/export.py`).
    #[test]
    fn matches_the_training_code() {
        let v = include_bytes!("denoise/vector.bin");
        let n = u32::from_le_bytes(v[..4].try_into().unwrap()) as usize;
        let input = floats(&v[4..4 + 4 * n]);
        let want = floats(&v[4 + 4 * n..]);
        let mut d = Denoiser::new();
        let mut got = Vec::with_capacity(n);
        for chunk in input.chunks_exact(BLOCK) {
            let mut block: [f32; BLOCK] = chunk.try_into().unwrap();
            d.process(&mut block, 0.8);
            got.extend_from_slice(&block);
        }
        let err: f32 = got.iter().zip(&want).map(|(a, b)| (a - b) * (a - b)).sum();
        let sig: f32 = want.iter().map(|x| x * x).sum();
        let snr = 10.0 * (sig / err.max(1e-20)).log10();
        assert!(snr > 50.0, "Rust and Python differ: {snr:.1} dB apart");
    }

    /// `cargo test --release -- --ignored cost`: CPU time per 2.5 ms block.
    #[test]
    #[ignore]
    fn cost() {
        let mut d = Denoiser::new();
        let mut block = [0.01f32; BLOCK];
        let t = std::time::Instant::now();
        for _ in 0..4000 {
            d.process(&mut block, 1.0);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / 4000.0;
        println!("{us:.1} us per block ({:.1}% of the 2.5 ms budget)", us / 25.0);
    }

    #[test]
    fn zero_amount_is_an_exact_passthrough() {
        let mut d = Denoiser::new();
        let mut seed = 1u32;
        for _ in 0..50 {
            let input: [f32; BLOCK] = std::array::from_fn(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            });
            let mut block = input;
            d.process(&mut block, 0.0);
            for (a, b) in block.iter().zip(&input) {
                assert!((a - b).abs() < 1e-4, "{a} vs {b}");
            }
        }
    }
}

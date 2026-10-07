//! Bulk delay estimation with GCC-PHAT.
//!
//! Echo paths here contain large pure delays (device buffers, network,
//! jitter buffer, air). Estimating that delay lets the adaptive filter spend
//! its taps on the room response instead of on leading zeros.

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use super::{History, energy};

pub struct DelayEstimator {
    window: usize,
    max_lag: usize,
    reference: History,
    target: History,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
    ref_buf: Vec<f32>,
    tgt_buf: Vec<f32>,
    ref_spec: Vec<Complex32>,
    tgt_spec: Vec<Complex32>,
    corr: Vec<f32>,
    candidate: Option<usize>,
    current: Option<usize>,
    confidence: f32,
}

impl DelayEstimator {
    /// `window`: analysis length; `max_lag`: largest detectable delay (samples).
    pub fn new(window: usize, max_lag: usize) -> Self {
        let n = (window + max_lag).next_power_of_two();
        let mut planner = RealFftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        Self {
            window,
            max_lag,
            reference: History::new(window + max_lag),
            target: History::new(window),
            ref_spec: fwd.make_output_vec(),
            tgt_spec: fwd.make_output_vec(),
            ref_buf: vec![0.0; n],
            tgt_buf: vec![0.0; n],
            corr: vec![0.0; n],
            fwd,
            inv,
            candidate: None,
            current: None,
            confidence: 0.0,
        }
    }

    pub fn push(&mut self, reference: &[f32], target: &[f32]) {
        for &r in reference {
            self.reference.push(r);
        }
        for &t in target {
            self.target.push(t);
        }
    }

    /// Accepted delay: `target[n] ≈ h * reference[n - delay]`.
    pub fn delay(&self) -> Option<usize> {
        self.current
    }

    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    /// Runs one correlation. Returns `true` when the accepted delay changed.
    pub fn update(&mut self) -> bool {
        let span = self.window + self.max_lag;

        self.ref_buf.fill(0.0);
        self.reference.copy_recent(&mut self.ref_buf[..span]);
        self.tgt_buf.fill(0.0);
        self.target.copy_recent(&mut self.tgt_buf[..self.window]);

        // Need actual signal in both, otherwise the correlation is noise.
        let er = energy(&self.ref_buf[self.max_lag..span]) / self.window as f32;
        let et = energy(&self.tgt_buf[..self.window]) / self.window as f32;
        if er < 1e-6 || et < 1e-7 {
            return false;
        }

        if self.fwd.process(&mut self.ref_buf, &mut self.ref_spec).is_err()
            || self.fwd.process(&mut self.tgt_buf, &mut self.tgt_spec).is_err()
        {
            return false;
        }

        // PHAT weighting: whiten so the peak is sharp regardless of spectra.
        let mut mean_mag = 0.0;
        for (r, t) in self.ref_spec.iter_mut().zip(&self.tgt_spec) {
            *r = t.conj() * *r;
            mean_mag += r.norm();
        }
        mean_mag /= self.ref_spec.len() as f32;
        let floor = mean_mag * 1e-2 + 1e-12;
        for c in self.ref_spec.iter_mut() {
            *c /= c.norm() + floor;
        }
        self.ref_spec[0] = Complex32::new(0.0, 0.0);
        if let Some(last) = self.ref_spec.last_mut() {
            last.im = 0.0;
        }
        if self.inv.process(&mut self.ref_spec, &mut self.corr).is_err() {
            return false;
        }

        // corr[m] = Σ tgt[j]·ref[m + j]; target sits at the end of ref's span,
        // so delay = max_lag - m.
        let range = &self.corr[..=self.max_lag];
        let (best_m, peak) = range
            .iter()
            .enumerate()
            .fold((0, f32::MIN), |acc, (i, &v)| if v > acc.1 { (i, v) } else { acc });
        let mean_abs = range.iter().map(|v| v.abs()).sum::<f32>() / range.len() as f32;
        let ratio = peak / (mean_abs + 1e-12);

        let lag = self.max_lag - best_m;
        self.confidence = 0.8 * self.confidence + 0.2 * (ratio / 20.0).min(1.0);
        if ratio < 12.0 {
            return false;
        }
        // Require two consecutive agreeing estimates before switching.
        let agrees = |a: usize, b: usize| a.abs_diff(b) <= 16;
        let prev = self.candidate.replace(lag);
        match (prev, self.current) {
            (Some(p), cur) if agrees(p, lag) => {
                if cur.is_none_or(|c| !agrees(c, lag)) {
                    self.current = Some(lag);
                    return true;
                }
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::nlms::tests::noise;

    #[test]
    fn finds_delay() {
        let delay = 1234;
        let mut est = DelayEstimator::new(4096, 4096);
        let mut seed = 99;
        let src: Vec<f32> = (0..40_000).map(|_| noise(&mut seed)).collect();
        let mut changed = false;
        for start in (0..src.len()).step_by(120) {
            let end = (start + 120).min(src.len());
            let tgt: Vec<f32> = (start..end)
                .map(|i| if i >= delay { 0.3 * src[i - delay] + 0.05 * noise(&mut seed) } else { 0.0 })
                .collect();
            est.push(&src[start..end], &tgt);
            if start % 4800 == 0 && start > 10_000 {
                changed |= est.update();
            }
        }
        assert!(changed);
        let got = est.delay().unwrap();
        assert!(got.abs_diff(delay) <= 2, "estimated {got}");
    }
}

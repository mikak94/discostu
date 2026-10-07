//! Normalized LMS FIR adaptive filter.

use super::{axpy, dot};

pub struct Nlms {
    taps: usize,
    pub w: Vec<f32>,
    /// Double-length history so the window is always one contiguous slice:
    /// every sample is written at `pos` and `pos + taps`.
    hist: Vec<f32>,
    pos: usize,
    energy: f32,
    pushes: usize,
}

impl Nlms {
    pub fn new(taps: usize) -> Self {
        assert!(taps > 0 && taps % 8 == 0, "taps must be a positive multiple of 8");
        Self {
            taps,
            w: vec![0.0; taps],
            hist: vec![0.0; taps * 2],
            pos: 0,
            energy: 0.0,
            pushes: 0,
        }
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        let oldest = self.hist[self.pos + self.taps - 1];
        self.pos = if self.pos == 0 { self.taps - 1 } else { self.pos - 1 };
        self.hist[self.pos] = x;
        self.hist[self.pos + self.taps] = x;
        self.energy += x * x - oldest * oldest;
        self.pushes += 1;
        if self.pushes >= 4096 {
            // Recompute to cancel accumulated rounding drift.
            self.pushes = 0;
            self.energy = dot(self.window(), self.window());
        }
    }

    /// Input window, newest sample first.
    #[inline]
    pub fn window(&self) -> &[f32] {
        &self.hist[self.pos..self.pos + self.taps]
    }

    /// Mean input power over the window.
    pub fn power(&self) -> f32 {
        self.energy.max(0.0) / self.taps as f32
    }

    #[inline]
    pub fn filter(&self) -> f32 {
        dot(&self.w, self.window())
    }

    #[inline]
    pub fn filter_with(&self, w: &[f32]) -> f32 {
        dot(w, self.window())
    }

    #[inline]
    pub fn adapt(&mut self, error: f32, mu: f32) {
        let norm = self.energy.max(0.0) + 1e-6 * self.taps as f32;
        let k = mu * error / norm;
        let (w, hist, pos, taps) = (&mut self.w, &self.hist, self.pos, self.taps);
        axpy(w, k, &hist[pos..pos + taps]);
    }

    pub fn reset_history(&mut self) {
        self.hist.fill(0.0);
        self.energy = 0.0;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Deterministic white-ish noise.
    pub fn noise(seed: &mut u32) -> f32 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 17;
        *seed ^= *seed << 5;
        (*seed as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    #[test]
    fn identifies_fir_system() {
        let h = [0.0, 0.5, -0.3, 0.2, 0.0, 0.1, 0.0, 0.05];
        let mut f = Nlms::new(16);
        let mut x_hist = [0f32; 8];
        let mut seed = 0x1234_5678;
        let mut err = 0.0;
        for n in 0..20_000 {
            let x = noise(&mut seed);
            x_hist.rotate_right(1);
            x_hist[0] = x;
            let d: f32 = h.iter().zip(&x_hist).map(|(a, b)| a * b).sum();
            f.push(x);
            let e = d - f.filter();
            f.adapt(e, 0.5);
            if n > 19_000 {
                err += e * e;
            }
        }
        assert!(err / 1000.0 < 1e-6, "residual {err}");
        for (i, &hi) in h.iter().enumerate() {
            assert!((f.w[i] - hi).abs() < 1e-3);
        }
    }
}

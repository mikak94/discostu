//! Low-latency, sample-domain voice DSP.
//!
//! Nothing here buffers audio beyond the 2.5 ms block it is handed: all
//! frequency-domain work (delay estimation, voice analysis) runs on side
//! chains that only steer gains and filter alignment.

pub mod delay;
pub mod denoise;
pub mod echo;
pub mod nlms;
pub mod profile;
pub mod talker;

/// Dot product with 8 independent accumulators so LLVM emits SIMD.
#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut acc = [0f32; 8];
    let ca = a.chunks_exact(8);
    let cb = b.chunks_exact(8);
    let (ra, rb) = (ca.remainder(), cb.remainder());
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut sum: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        sum += x * y;
    }
    sum
}

/// `y += k * x`
#[inline]
pub fn axpy(y: &mut [f32], k: f32, x: &[f32]) {
    for (yi, xi) in y.iter_mut().zip(x) {
        *yi += k * xi;
    }
}

#[inline]
pub fn energy(x: &[f32]) -> f32 {
    dot(x, x)
}

pub fn db(power: f32) -> f32 {
    10.0 * (power.max(1e-12)).log10()
}

/// Soft limiter: transparent below 0.8, a continuous knee into ±1.0, so peaks
/// round off instead of clipping flat when converted to 16 bits.
#[inline]
pub fn soft_limit(s: f32) -> f32 {
    let a = s.abs();
    if a > 0.8 { s.signum() * (0.8 + 0.2 * ((a - 0.8) / 0.2).tanh()) } else { s }
}

/// One-pole smoothing coefficient for a time constant expressed in blocks.
pub fn coeff(blocks: f32) -> f32 {
    1.0 - (-1.0 / blocks.max(1e-3)).exp()
}

/// Fixed-capacity history of the most recent samples, indexable by age.
pub struct History {
    buf: Vec<f32>,
    pos: usize,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self { buf: vec![0.0; capacity.next_power_of_two()], pos: 0 }
    }

    #[inline]
    pub fn push(&mut self, x: f32) {
        self.pos = (self.pos + 1) & (self.buf.len() - 1);
        self.buf[self.pos] = x;
    }

    /// Sample pushed `age` pushes ago (0 = newest).
    #[inline]
    pub fn get(&self, age: usize) -> f32 {
        self.buf[(self.pos.wrapping_sub(age)) & (self.buf.len() - 1)]
    }

    /// Copies the `out.len()` most recent samples, oldest first.
    pub fn copy_recent(&self, out: &mut [f32]) {
        let n = out.len();
        for (i, o) in out.iter_mut().enumerate() {
            *o = self.get(n - 1 - i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..37).map(|i| i as f32 * 0.5).collect();
        let b: Vec<f32> = (0..37).map(|i| 1.0 - i as f32 * 0.1).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-3);
    }

    #[test]
    fn history_ages() {
        let mut h = History::new(8);
        for i in 0..20 {
            h.push(i as f32);
        }
        assert_eq!(h.get(0), 19.0);
        assert_eq!(h.get(3), 16.0);
        let mut out = [0.0; 4];
        h.copy_recent(&mut out);
        assert_eq!(out, [16.0, 17.0, 18.0, 19.0]);
    }
}

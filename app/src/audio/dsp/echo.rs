//! Delay-compensated two-filter echo canceller with residual suppression.
//!
//! Used twice with different references:
//! - **AEC**: reference = what we send to the speakers, target = our mic.
//! - **Crosstalk**: reference = our own mic, target = a peer's incoming
//!   stream. When we share a room, our voice reaches their mic acoustically
//!   and would come back to us a few ms later. Our mic signal always leads
//!   that copy, so the filter stays causal.
//!
//! Two-filter scheme (background adapts fast, foreground only takes the
//! background's weights when they demonstrably cancel better) makes the
//! filter robust against double talk without a fragile detector.

use super::delay::DelayEstimator;
use super::nlms::Nlms;
use super::{History, coeff, energy};

/// Taps placed before the estimated bulk delay to absorb estimation error.
const PRE_TAPS: usize = 32;

pub struct EchoCanceller {
    reference: History,
    estimator: DelayEstimator,
    blocks_until_estimate: u32,
    delay: Option<usize>,
    bg: Nlms,
    fg: Vec<f32>,
    mu: f32,
    // Smoothed statistics.
    leak: f32,
    coupling: f32,
    gain: f32,
    /// 0 = filtered output, 1 = target passed through (crossfaded).
    bypass: f32,
    echo_ratio: f32,
    scratch: Vec<f32>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EchoStats {
    /// Bulk delay currently compensated, in samples.
    pub delay: Option<usize>,
    /// Fraction of target energy explained by the reference (0..1).
    pub coupling: f32,
    /// Residual echo relative to output after linear cancellation (0..1+).
    pub echo_ratio: f32,
}

impl EchoCanceller {
    pub fn new(taps: usize, max_delay: usize) -> Self {
        Self {
            reference: History::new(max_delay + taps + 4096),
            estimator: DelayEstimator::new(4096, max_delay),
            blocks_until_estimate: 40,
            delay: None,
            bg: Nlms::new(taps),
            fg: vec![0.0; taps],
            mu: 0.4,
            leak: 1.0,
            coupling: 0.0,
            gain: 1.0,
            bypass: 1.0,
            echo_ratio: 0.0,
            scratch: Vec::new(),
        }
    }

    pub fn stats(&self) -> EchoStats {
        EchoStats {
            delay: self.delay,
            coupling: self.coupling,
            echo_ratio: self.echo_ratio,
        }
    }

    fn lag(&self) -> usize {
        self.delay.map_or(0, |d| d.saturating_sub(PRE_TAPS))
    }

    fn realign(&mut self, delay: usize) {
        let old = self.lag();
        self.delay = Some(delay);
        let new = self.lag();
        // Shift weights by the delay change when it is small, otherwise restart.
        let taps = self.fg.len();
        let shift = old as isize - new as isize;
        if shift.unsigned_abs() < taps / 2 {
            for w in [&mut self.fg, &mut self.bg.w] {
                let mut shifted = vec![0.0; taps];
                for (i, &v) in w.iter().enumerate() {
                    let j = i as isize + shift;
                    if (0..taps as isize).contains(&j) {
                        shifted[j as usize] = v;
                    }
                }
                *w = shifted;
            }
        } else {
            self.fg.fill(0.0);
            self.bg.w.fill(0.0);
        }
        // Refill the filter window from history at the new alignment.
        self.bg.reset_history();
        for age in (0..taps).rev() {
            self.bg.push(self.reference.get(new + age));
        }
    }

    /// Cancels echo of `reference` from `target` in place. Both are one block,
    /// captured at the same tick.
    ///
    /// `active`: the reference source is known to be live (e.g. we are
    /// talking). While inactive, history stays current but nothing adapts or
    /// is subtracted, and the delay estimator sees silence — so a reference
    /// that also picks up *other* sources can't train the filter on them.
    /// `suppress` enables the non-linear residual stage.
    pub fn process(&mut self, reference: &[f32], target: &mut [f32], active: bool, suppress: bool) {
        debug_assert_eq!(reference.len(), target.len());
        if active {
            self.estimator.push(reference, target);
        } else {
            let zeros = [0f32; 512];
            self.estimator.push(&zeros[..reference.len()], &zeros[..target.len()]);
        }
        self.blocks_until_estimate = self.blocks_until_estimate.saturating_sub(1);
        if self.blocks_until_estimate == 0 {
            self.blocks_until_estimate = 40; // every 100 ms
            if self.estimator.update()
                && let Some(d) = self.estimator.delay()
            {
                self.realign(d);
            }
        }

        let ref_energy = energy(reference);
        let Some(_) = self.delay else {
            // Nothing correlates yet: keep history current, leave target alone.
            for &r in reference {
                self.reference.push(r);
            }
            self.coupling *= 0.999;
            self.echo_ratio = 0.0;
            self.gain = 1.0;
            self.bypass = 1.0;
            return;
        };

        let lag = self.lag();
        let adapt = active && self.bg.power() > 1e-7;
        self.scratch.clear();
        self.scratch.extend_from_slice(target);
        let (mut e_d, mut e_bg, mut e_fg, mut e_y) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        for (i, &r) in reference.iter().enumerate() {
            self.reference.push(r);
            self.bg.push(self.reference.get(lag));
            let d = target[i];
            let y_fg = self.bg.filter_with(&self.fg);
            let err_fg = d - y_fg;
            if adapt {
                let err_bg = d - self.bg.filter();
                // Clip the error so a burst of near-end speech can't blow it up.
                let limit = 4.0 * self.bg.power().sqrt() + 1e-3;
                self.bg.adapt(err_bg.clamp(-limit, limit), self.mu);
                e_bg += err_bg * err_bg;
            }
            e_d += d * d;
            e_fg += err_fg * err_fg;
            e_y += y_fg * y_fg;
            target[i] = err_fg;
        }

        // Foreground/background arbitration.
        if adapt {
            if e_bg < 0.7 * e_fg && e_bg < e_d {
                self.fg.copy_from_slice(&self.bg.w);
            } else if e_bg > 4.0 * e_d && e_fg < e_bg {
                self.bg.w.copy_from_slice(&self.fg);
            }
        }

        // Pass the target through while the reference is idle or the filter
        // would make things worse — crossfading so the switch never clicks.
        let want = if !active || e_fg > e_d * 1.05 { 1.0 } else { 0.0 };
        if want > 0.5 {
            e_fg = e_d;
        }
        let n = target.len() as f32;
        let start = self.bypass;
        if start != want || want > 0.5 {
            for (i, (t, d)) in target.iter_mut().zip(&self.scratch).enumerate() {
                let m = start + (want - start) * (i as f32 + 1.0) / n;
                *t = *t * (1.0 - m) + d * m;
            }
        }
        self.bypass = want;

        let live = active && ref_energy > 1e-6 * reference.len() as f32 && e_d > 1e-9;
        if live {
            // Coupling: how much of the target the reference explains.
            let explained = (1.0 - e_fg / e_d).clamp(0.0, 1.0);
            self.coupling += coeff(400.0) * (explained - self.coupling);
            // Leak: residual floor during single talk, tracked as a fast-down /
            // slow-up minimum of e_fg/e_y.
            if e_y > 1e-9 {
                let ratio = (e_fg / e_y).clamp(1e-3, 1.0);
                if ratio < self.leak {
                    self.leak += 0.3 * (ratio - self.leak);
                } else {
                    self.leak += 0.002 * (ratio - self.leak);
                }
            }
        } else {
            self.coupling *= 0.9998;
        }

        // Residual suppression: estimated leftover echo = leak · e_y. Only once
        // the filter has demonstrably converged, and never below -14 dB, so
        // double talk is softened rather than chopped.
        let residual = self.leak * e_y;
        self.echo_ratio = if live { residual / (e_fg + 1e-10) } else { 0.0 };
        let converged = self.leak < 0.3 && self.coupling > 0.05;
        let target_gain = if suppress && live && converged && want < 0.5 {
            ((e_fg - residual) / (e_fg + 1e-10)).clamp(0.2, 1.0)
        } else {
            1.0
        };
        let k = if target_gain < self.gain { 0.3 } else { coeff(20.0) };
        let start = self.gain;
        self.gain += k * (target_gain - self.gain);
        ramp(target, start, self.gain);
    }
}

/// Applies a linear gain ramp from `from` to `to` across the block.
pub fn ramp(block: &mut [f32], from: f32, to: f32) {
    let n = block.len() as f32;
    for (i, s) in block.iter_mut().enumerate() {
        *s *= from + (to - from) * (i as f32 + 1.0) / n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::nlms::tests::noise;
    use crate::protocol::BLOCK;

    /// Speech-like signal: noise shaped by a slowly varying envelope.
    fn source(seed: &mut u32, n: usize) -> Vec<f32> {
        let mut lp = 0.0;
        (0..n)
            .map(|i| {
                lp = 0.7 * lp + 0.3 * noise(seed);
                let env = 0.5 + 0.5 * ((i as f32) * 0.0007).sin();
                lp * env
            })
            .collect()
    }

    #[test]
    fn cancels_delayed_room_echo() {
        let mut seed = 7;
        let n = BLOCK * 3000; // 7.5 s
        let reference = source(&mut seed, n);
        let delay = 900;
        let room = [0.4, 0.0, -0.2, 0.1, 0.0, 0.05, -0.03, 0.02];
        let echo: Vec<f32> = (0..n)
            .map(|i| {
                room.iter()
                    .enumerate()
                    .map(|(k, h)| if i >= delay + k { h * reference[i - delay - k] } else { 0.0 })
                    .sum()
            })
            .collect();

        let mut aec = EchoCanceller::new(256, 4096);
        let (mut in_e, mut out_e) = (0.0, 0.0);
        for b in 0..n / BLOCK {
            let r = &reference[b * BLOCK..(b + 1) * BLOCK];
            let mut t = echo[b * BLOCK..(b + 1) * BLOCK].to_vec();
            if b > 2500 {
                in_e += energy(&t);
            }
            aec.process(r, &mut t, true, false);
            if b > 2500 {
                out_e += energy(&t);
            }
        }
        let erle = 10.0 * (in_e / out_e).log10();
        assert!(aec.stats().delay.is_some());
        assert!(erle > 25.0, "ERLE only {erle:.1} dB");
        assert!(aec.stats().coupling > 0.5);
    }

    #[test]
    fn leaves_uncorrelated_audio_alone() {
        let mut seed = 3;
        let n = BLOCK * 1200;
        let reference = source(&mut seed, n);
        let other = source(&mut seed, n);
        let mut aec = EchoCanceller::new(256, 4096);
        let (mut in_e, mut out_e) = (0.0, 0.0);
        for b in 0..n / BLOCK {
            let r = &reference[b * BLOCK..(b + 1) * BLOCK];
            let mut t = other[b * BLOCK..(b + 1) * BLOCK].to_vec();
            in_e += energy(&t);
            aec.process(r, &mut t, true, true);
            out_e += energy(&t);
        }
        assert!(out_e / in_e > 0.9, "attenuated uncorrelated speech: {}", out_e / in_e);
    }
}

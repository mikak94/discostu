//! Adaptive jitter buffer for fixed-size audio blocks (`N` interleaved samples).
//!
//! Targets the smallest depth that avoids underruns: starts at one block,
//! grows on underruns or measured jitter, and shrinks back after a quiet
//! period. Every discontinuity it introduces is smoothed:
//! - drift correction drops a block by crossfading it into the next one,
//! - concealment fades out, and real audio fades back in afterwards.

use std::collections::VecDeque;

use crate::protocol::{BLOCK, SAMPLE_RATE};

const BLOCK_US: f32 = BLOCK as f32 * 1e6 / SAMPLE_RATE as f32;
const MAX_DEPTH: usize = 40; // 100 ms
/// Cap on one packet's timing deviation (20 ms).
const MAX_DEVIATION_US: f32 = 20_000.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub target: usize,
    pub jitter_us: f32,
    pub underruns: u64,
    pub lost: u64,
    pub late: u64,
    /// Blocks dropped to catch up with a faster sender clock.
    pub drops: u64,
}

pub struct JitterBuffer<const N: usize> {
    queue: VecDeque<(u32, [f32; N])>,
    next_seq: Option<u32>,
    playing: bool,
    target: usize,
    /// Never aim below this many blocks (bursty senders).
    min_target: usize,
    last: [f32; N],
    conceal_gain: f32,
    concealing: bool,
    last_arrival: Option<(u32, u64)>,
    jitter_us: f32,
    stable_blocks: u32,
    over_target_blocks: u32,
    channels: usize,
    stats: JitterStats,
}

impl<const N: usize> JitterBuffer<N> {
    /// `channels`: interleaved channel count within each block.
    pub fn new(channels: usize) -> Self {
        Self {
            queue: VecDeque::with_capacity(MAX_DEPTH + 4),
            next_seq: None,
            playing: false,
            target: 1,
            min_target: 1,
            last: [0.0; N],
            conceal_gain: 0.0,
            concealing: false,
            last_arrival: None,
            jitter_us: 0.0,
            stable_blocks: 0,
            over_target_blocks: 0,
            channels,
            stats: JitterStats::default(),
        }
    }

    /// Keeps at least `blocks` queued, e.g. a sender that delivers in 10 ms
    /// bursts needs 4 to play smoothly.
    pub fn with_min_target(mut self, blocks: usize) -> Self {
        self.min_target = blocks.max(1);
        self.target = self.target.max(self.min_target);
        self
    }

    /// Blocks queued right now.
    pub fn depth(&self) -> usize {
        self.queue.len()
    }

    pub fn stats(&self) -> JitterStats {
        JitterStats { target: self.target, jitter_us: self.jitter_us, ..self.stats }
    }

    /// Inserts a block; `arrival_us` is local receive time.
    pub fn push(&mut self, seq: u32, block: [f32; N], arrival_us: u64) {
        // RFC 3550-style interarrival jitter.
        // Gaps (loss, a sender pause) and single stalls are not jitter: skip
        // the former, cap the latter so one hiccup can't max out the depth.
        if let Some((pseq, pt)) = self.last_arrival
            && seq > pseq
            && seq - pseq <= 8
        {
            let expected = (seq - pseq) as f32 * BLOCK_US;
            let actual = arrival_us.saturating_sub(pt) as f32;
            let deviation = (actual - expected).abs().min(MAX_DEVIATION_US);
            self.jitter_us += (deviation - self.jitter_us) / 16.0;
        }
        if self.last_arrival.is_none_or(|(p, _)| seq > p) {
            self.last_arrival = Some((seq, arrival_us));
        }

        if let Some(next) = self.next_seq
            && seq < next
        {
            // A sender restart resets its sequence: resync instead of
            // discarding everything as "late".
            if next - seq > 1000 {
                self.queue.clear();
                self.next_seq = None;
                self.playing = false;
            } else {
                self.stats.late += 1;
                return;
            }
        }
        // Keep sorted; packets almost always arrive in order, so scan from back.
        let idx = self.queue.iter().rposition(|(s, _)| *s < seq).map_or(0, |i| i + 1);
        if self.queue.get(idx).is_some_and(|(s, _)| *s == seq) {
            return; // duplicate
        }
        self.queue.insert(idx, (seq, block));
        while self.queue.len() > MAX_DEPTH {
            self.queue.pop_front();
            if let Some((s, _)) = self.queue.front() {
                self.next_seq = Some(*s);
            }
        }
    }

    /// Produces the next block to play (concealment on loss/underrun).
    pub fn pop(&mut self) -> [f32; N] {
        if !self.playing {
            if self.queue.len() >= self.target {
                self.playing = true;
                self.next_seq = self.queue.front().map(|(s, _)| *s);
            } else {
                return self.conceal();
            }
        }

        let Some(next) = self.next_seq else {
            return self.conceal();
        };

        match self.queue.front() {
            Some((s, _)) if *s == next => {
                let (_, mut block) = self.queue.pop_front().expect("front exists");
                self.next_seq = Some(next.wrapping_add(1));
                if self.drift_drop() {
                    self.stats.drops += 1;
                    // Crossfade this block into the following one: playback
                    // skips 2.5 ms without a seam.
                    if let Some((s2, b2)) = self.queue.pop_front() {
                        self.next_seq = Some(s2.wrapping_add(1));
                        crossfade(&mut block, &b2, self.channels);
                    }
                }
                if self.concealing {
                    ramp(&mut block, self.conceal_gain, 1.0, self.channels);
                    self.concealing = false;
                }
                self.last = block;
                self.conceal_gain = 1.0;
                self.adapt();
                block
            }
            Some(_) => {
                // Gap: the expected packet is missing but later ones are here.
                self.stats.lost += 1;
                self.next_seq = Some(next.wrapping_add(1));
                self.conceal()
            }
            None => {
                self.stats.underruns += 1;
                self.playing = false;
                self.stable_blocks = 0;
                self.target = (self.target + 1).min(MAX_DEPTH / 2);
                self.conceal()
            }
        }
    }

    fn adapt(&mut self) {
        let jitter_target = (((2.0 * self.jitter_us) / BLOCK_US).ceil() as usize).max(self.min_target);
        self.stable_blocks += 1;
        if jitter_target > self.target {
            self.target = jitter_target.min(MAX_DEPTH / 2);
            self.stable_blocks = 0;
        } else if self.target > jitter_target + 1 && self.stable_blocks > 100 {
            // Well above what the jitter needs (left over from a burst):
            // come down a block every 250 ms.
            self.target -= 1;
            self.stable_blocks = 0;
        } else if self.stable_blocks > 4000 && self.target > jitter_target {
            // 10 s without trouble: try one block less. Shorter, and bursty
            // senders (10 ms shared-mode capture) make it cycle: shrink, run
            // dry, conceal, grow, shrink again.
            self.target -= 1;
            self.stable_blocks = 0;
        }
    }

    /// Persistently deeper than needed (clock drift, or a burst that has
    /// passed): ask for one block to be dropped.
    fn drift_drop(&mut self) -> bool {
        if self.queue.len() > self.target + 1 {
            self.over_target_blocks += 1;
            if self.over_target_blocks > 40 {
                self.over_target_blocks = 0;
                return true;
            }
        } else {
            self.over_target_blocks = 0;
        }
        false
    }

    fn conceal(&mut self) -> [f32; N] {
        // Repeat the last block with a fast fade so short gaps are smoothed
        // and long ones decay to silence.
        let mut out = self.last;
        let start = self.conceal_gain;
        self.conceal_gain *= 0.5;
        if self.conceal_gain < 0.01 {
            self.conceal_gain = 0.0;
        }
        ramp(&mut out, start, self.conceal_gain, self.channels);
        self.last = out;
        self.concealing = true;
        out
    }
}

/// Linear gain ramp across an interleaved block.
pub fn ramp(block: &mut [f32], from: f32, to: f32, channels: usize) {
    let frames = (block.len() / channels) as f32;
    for (f, frame) in block.chunks_exact_mut(channels).enumerate() {
        let g = from + (to - from) * (f as f32 + 1.0) / frames;
        for s in frame {
            *s *= g;
        }
    }
}

/// `a` fades out while `b` fades in, result in `a`.
fn crossfade(a: &mut [f32], b: &[f32], channels: usize) {
    let frames = (a.len() / channels) as f32;
    for (f, (fa, fb)) in a.chunks_exact_mut(channels).zip(b.chunks_exact(channels)).enumerate() {
        let w = (f as f32 + 0.5) / frames;
        for (x, y) in fa.iter_mut().zip(fb) {
            *x = *x * (1.0 - w) + y * w;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Jb = JitterBuffer<BLOCK>;

    fn blk(v: f32) -> [f32; BLOCK] {
        [v; BLOCK]
    }

    #[test]
    fn in_order_playback() {
        let mut jb = Jb::new(1);
        for s in 0..5 {
            jb.push(s, blk(s as f32), s as u64 * 2500);
        }
        for s in 0..5 {
            assert_eq!(jb.pop()[0], s as f32);
        }
    }

    #[test]
    fn reorders_and_conceals_loss() {
        let mut jb = Jb::new(1);
        jb.push(0, blk(1.0), 0);
        jb.push(2, blk(3.0), 5000);
        jb.push(1, blk(2.0), 5100);
        jb.push(4, blk(5.0), 10000);
        assert_eq!(jb.pop()[0], 1.0);
        assert_eq!(jb.pop()[0], 2.0);
        assert_eq!(jb.pop()[0], 3.0);
        let concealed = jb.pop();
        assert_eq!(jb.stats().lost, 1);
        assert!(concealed[BLOCK - 1].abs() < 3.0);
        // Back from concealment: fades in rather than jumping.
        let back = jb.pop();
        assert!(back[0] < 5.0 && (back[BLOCK - 1] - 5.0).abs() < 1e-4);
    }

    #[test]
    fn one_stall_does_not_stick() {
        // Steady stream, one 300 ms network stall, then steady again.
        let mut jb = Jb::new(1);
        let mut t = 0u64;
        let mut peak = 0;
        for s in 0..2000u32 {
            t += if s == 400 { 300_000 } else { 2500 };
            jb.push(s, blk(1.0), t);
            jb.pop();
            peak = peak.max(jb.stats().target);
            if s == 1200 {
                // 2 s after the stall: back to a shallow buffer.
                assert!(jb.stats().target <= 3, "target {} (peak {peak})", jb.stats().target);
            }
        }
        assert!(peak < MAX_DEPTH / 2, "a single stall maxed out the buffer");
    }

    #[test]
    fn underrun_grows_target() {
        let mut jb = Jb::new(1);
        jb.push(0, blk(1.0), 0);
        jb.pop();
        jb.pop();
        assert_eq!(jb.stats().underruns, 1);
        assert_eq!(jb.stats().target, 2);
    }

    #[test]
    fn drift_drop_is_seamless() {
        let mut jb = Jb::new(1);
        // Ramp signal so a seam would show up as a jump.
        let mut seq = 0u32;
        let mut push = |jb: &mut Jb, seq: &mut u32| {
            let base = *seq as f32 * BLOCK as f32;
            jb.push(*seq, std::array::from_fn(|i| (base + i as f32) * 1e-4), *seq as u64 * 2500);
            *seq += 1;
        };
        for _ in 0..4 {
            push(&mut jb, &mut seq);
        }
        let mut prev_end: Option<f32> = None;
        for _ in 0..200 {
            push(&mut jb, &mut seq);
            let b = jb.pop();
            if let Some(p) = prev_end {
                assert!((b[0] - p).abs() < 0.03, "seam {p} -> {}", b[0]);
            }
            prev_end = Some(b[BLOCK - 1]);
        }
    }
}

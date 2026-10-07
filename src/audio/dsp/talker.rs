//! Who is talking in a shared room: us, them, or both.
//!
//! Whoever talks is inches from their own mic and across the room from the
//! other one, so the level difference between our mic and their stream swings
//! by 15–25 dB depending on who speaks. Mic gains differ, so rather than a
//! fixed threshold we learn the two extremes of that difference (us alone,
//! them alone) and classify against them.

use super::{coeff, db};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Talker {
    /// Nobody talking, or not calibrated yet (needs each side alone once).
    Unknown,
    Us,
    Them,
    Both,
}

pub struct TalkerDetector {
    mine: f32,
    theirs: f32,
    /// Level difference (ours − theirs, dB) when we talk alone / they do.
    hi: f32,
    lo: f32,
    seeded: bool,
}

/// The two clusters must be at least this far apart to trust a decision.
const MIN_SPREAD_DB: f32 = 12.0;
/// Below this, nobody is loud enough to judge.
const ACTIVE_DB: f32 = -60.0;
/// How slowly the learned extremes relax toward each other (dB per block).
const FORGET_DB: f32 = 0.0005;

impl TalkerDetector {
    pub fn new() -> Self {
        Self { mine: 0.0, theirs: 0.0, hi: 0.0, lo: 0.0, seeded: false }
    }

    /// `mine`, `theirs`: mean power of one block of our mic and their stream.
    /// `speech`: someone is talking (our VAD or their voice flag).
    pub fn update(&mut self, mine: f32, theirs: f32, speech: bool) -> Talker {
        // ~20 ms envelopes: smooth over syllables, short enough to follow turns.
        self.mine += coeff(8.0) * (mine - self.mine);
        self.theirs += coeff(8.0) * (theirs - self.theirs);
        if !speech {
            return Talker::Unknown;
        }
        let (m, t) = (db(self.mine), db(self.theirs));
        if m.max(t) < ACTIVE_DB {
            return Talker::Unknown;
        }
        let diff = m - t;
        if !self.seeded {
            (self.hi, self.lo, self.seeded) = (diff, diff, true);
        }
        // Extremes: quick to reach, slow to forget.
        if diff > self.hi {
            self.hi += 0.05 * (diff - self.hi);
        } else {
            self.hi -= FORGET_DB;
        }
        if diff < self.lo {
            self.lo += 0.05 * (diff - self.lo);
        } else {
            self.lo += FORGET_DB;
        }
        let spread = self.hi - self.lo;
        if spread < MIN_SPREAD_DB {
            Talker::Unknown
        } else if diff < self.lo + 0.3 * spread {
            Talker::Them
        } else if diff > self.hi - 0.3 * spread {
            Talker::Us
        } else {
            Talker::Both
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(db: f32) -> f32 {
        10f32.powf(db / 10.0)
    }

    /// Feeds `blocks` blocks of steady levels, returns the last verdict.
    fn run(d: &mut TalkerDetector, mine_db: f32, theirs_db: f32, blocks: usize) -> Talker {
        let mut last = Talker::Unknown;
        for _ in 0..blocks {
            last = d.update(p(mine_db), p(theirs_db), true);
        }
        last
    }

    #[test]
    fn learns_who_talks_despite_mismatched_gains() {
        // Our mic runs 8 dB hotter than theirs; 20 dB across the room.
        let mut d = TalkerDetector::new();
        assert_eq!(run(&mut d, -15.0, -43.0, 400), Talker::Unknown, "one side alone can't calibrate");
        assert_eq!(run(&mut d, -35.0, -23.0, 400), Talker::Them);
        assert_eq!(run(&mut d, -15.0, -43.0, 400), Talker::Us);
        assert_eq!(run(&mut d, -15.0, -23.0, 400), Talker::Both);
        assert_eq!(run(&mut d, -35.0, -23.0, 400), Talker::Them);
    }

    #[test]
    fn silence_is_unknown() {
        let mut d = TalkerDetector::new();
        run(&mut d, -15.0, -43.0, 400);
        run(&mut d, -35.0, -23.0, 400);
        assert_eq!(d.update(p(-90.0), p(-90.0), false), Talker::Unknown);
    }
}

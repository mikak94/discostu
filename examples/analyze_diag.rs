//! Finds clicks in a diagnostics folder saved from Settings → "Save last 10 s".
//!
//! `cargo run --release --example analyze_diag -- <folder>`
//!
//! For each stage (mic in, sent, every peer as received, playback) it lists
//! sharp discontinuities and dropouts, and next to them what the log says
//! happened then: playback underruns, jitter-buffer gaps, gate and talker
//! changes. The first stage a click shows up in is where it was made.

use std::fs;
use std::path::Path;

const RATE: f32 = 48_000.0;
const BLOCK: usize = 120;

fn main() {
    let dir = std::env::args().nth(1).expect("usage: analyze_diag <folder>");
    let dir = Path::new(&dir);
    let log = Log::load(&dir.join("log.csv"));
    if let Ok(peers) = fs::read_to_string(dir.join("peers.txt")) {
        print!("peers:\n{peers}");
    }

    let mut wavs: Vec<_> = fs::read_dir(dir)
        .expect("read folder")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "wav"))
        .collect();
    wavs.sort();
    for path in wavs {
        let x = read_wav(&path);
        let events = clicks(&x);
        println!(
            "\n{} — {:.1} s, rms {:.4}, {} event(s)",
            path.file_name().unwrap().to_string_lossy(),
            x.len() as f32 / RATE,
            rms(&x),
            events.len()
        );
        for (n, kind, size) in events.iter().take(40) {
            let block = n / BLOCK;
            println!("  {:>8.1} ms  {kind:<9} {size:.3}  {}", *n as f32 * 1000.0 / RATE, log.around(block));
        }
        if events.len() > 40 {
            println!("  … {} more", events.len() - 40);
        }
    }
    println!("\nlog totals: {}", log.totals());
}

fn read_wav(path: &Path) -> Vec<f32> {
    let b = fs::read(path).expect("read wav");
    b[44..].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

/// (sample index, kind, size). "jump": second difference far above its local
/// level (a step or a spike). "dropout": a run of exact zeros between audio.
fn clicks(x: &[f32]) -> Vec<(usize, &'static str, f32)> {
    let mut out = Vec::new();
    let d2: Vec<f32> = (2..x.len()).map(|n| (x[n] - 2.0 * x[n - 1] + x[n - 2]).abs()).collect();
    let win = 480; // 10 ms
    let mut last = 0usize;
    for n in win..d2.len().saturating_sub(win) {
        let around = (d2[n - win..n].iter().sum::<f32>() + d2[n + 1..n + win].iter().sum::<f32>()) / (2 * win) as f32;
        if d2[n] > 0.02 && d2[n] > 10.0 * around.max(1e-4) && n > last + 240 {
            out.push((n + 2, "jump", d2[n]));
            last = n;
        }
    }
    let mut run = 0usize;
    for n in 0..x.len() {
        if x[n] == 0.0 {
            run += 1;
            continue;
        }
        if run >= 24 && n > run + 480 {
            let before = rms(&x[n - run - 480..n - run]);
            let after = rms(&x[n..(n + 480).min(x.len())]);
            if before > 0.005 && after > 0.005 {
                out.push((n - run, "dropout", run as f32 * 1000.0 / RATE));
            }
        }
        run = 0;
    }
    out.sort_by_key(|e| e.0);
    out
}

struct Log {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl Log {
    fn load(path: &Path) -> Self {
        let text = fs::read_to_string(path).unwrap_or_default();
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("").split(',').map(String::from).collect();
        let rows = lines.map(|l| l.split(',').map(String::from).collect()).collect();
        Self { header, rows }
    }

    fn col(&self, name: &str) -> Option<usize> {
        self.header.iter().position(|h| h == name)
    }

    fn num(&self, row: usize, name: &str) -> f64 {
        self.col(name)
            .and_then(|c| self.rows.get(row)?.get(c)?.parse().ok())
            .unwrap_or(0.0)
    }

    /// What changed in the 20 blocks (50 ms) before `block`.
    fn around(&self, block: usize) -> String {
        if self.rows.is_empty() {
            return String::new();
        }
        let b = block.min(self.rows.len() - 1);
        let a = b.saturating_sub(20);
        let mut notes = Vec::new();
        let delta = |name: &str| self.num(b, name) - self.num(a, name);
        if delta("out_underruns") > 0.0 {
            notes.push(format!("PLAYBACK UNDERRUN ×{}", delta("out_underruns")));
        }
        if delta("xruns") > 0.0 {
            notes.push("DEVICE XRUN".into());
        }
        for i in 0..3 {
            for (field, label) in [("underruns", "jitter underrun"), ("lost", "packet lost"), ("late", "late packet")] {
                let d = delta(&format!("p{i}_{field}"));
                if d > 0.0 {
                    notes.push(format!("p{i} {label} ×{d}"));
                }
            }
        }
        let flips = |name: &str| (a..b).filter(|&r| self.num(r, name) != self.num(r + 1, name)).count();
        if flips("roommate") > 0 {
            notes.push(format!("roommate flips {}", flips("roommate")));
        }
        for i in 0..3 {
            let f = flips(&format!("p{i}_talker"));
            if f > 0 {
                notes.push(format!("p{i} talker flips {f}"));
            }
        }
        notes.push(format!(
            "fill {} headroom {} gate {:.2}",
            self.num(b, "fill"),
            self.num(b, "headroom"),
            self.num(b, "gate")
        ));
        notes.join(" · ")
    }

    fn totals(&self) -> String {
        if self.rows.len() < 2 {
            return "empty".into();
        }
        let last = self.rows.len() - 1;
        let d = |name: &str| self.num(last, name) - self.num(0, name);
        let mut parts = vec![format!("playback underruns {}", d("out_underruns")), format!("xruns {}", d("xruns"))];
        for i in 0..3 {
            let id = self.col(&format!("p{i}_id")).and_then(|c| self.rows[last].get(c)).cloned().unwrap_or_default();
            if id.chars().all(|c| c == '0') {
                continue;
            }
            parts.push(format!(
                "p{i} {id}: jitter underruns {} lost {} late {}",
                d(&format!("p{i}_underruns")),
                d(&format!("p{i}_lost")),
                d(&format!("p{i}_late"))
            ));
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_dropout_and_a_step_in_a_tone() {
        let mut x: Vec<f32> = (0..48_000).map(|n| 0.3 * (n as f32 * 0.05).sin()).collect();
        for s in &mut x[12_000..12_120] {
            *s = 0.0; // 2.5 ms of silence: an underrun
        }
        for s in &mut x[30_000..] {
            *s += 0.2; // a step: an unsmoothed gain switch
        }
        let found = clicks(&x);
        assert!(found.iter().any(|(n, k, _)| *k == "dropout" && n.abs_diff(12_000) < 10), "{found:?}");
        assert!(found.iter().any(|(n, k, _)| *k == "jump" && n.abs_diff(30_000) < 10), "{found:?}");
        // A clean tone has nothing to report.
        let clean: Vec<f32> = (0..48_000).map(|n| 0.3 * (n as f32 * 0.05).sin()).collect();
        assert!(clicks(&clean).is_empty());
    }
}

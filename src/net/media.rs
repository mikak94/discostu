//! UDP: audio receive, and ping/pong for RTT, clock offset and liveness.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::clock;
use crate::engine::Engine;
use crate::protocol::{self, Datagram};

const PING_INTERVAL: Duration = Duration::from_millis(250);
const DEAD_AFTER_US: u64 = 6_000_000;

pub fn spawn(engine: Arc<Engine>) {
    let e = engine.clone();
    thread::Builder::new()
        .name("udp-recv".into())
        .spawn(move || {
            let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
            receive(e)
        })
        .expect("spawn udp receiver");
    thread::Builder::new().name("udp-ping".into()).spawn(move || ping(engine)).expect("spawn pinger");
}

fn receive(engine: Arc<Engine>) {
    let mut buf = [0u8; 2048];
    let mut out = Vec::with_capacity(64);
    loop {
        let Ok((n, from)) = engine.udp.recv_from(&mut buf) else { continue };
        match Datagram::parse(&buf[..n]) {
            Some(Datagram::Audio { sender, seq, flags, samples, .. }) => {
                engine.audio.receive(sender, seq, flags, samples);
            }
            Some(Datagram::Stream { sender, seq, samples }) => {
                engine.audio.receive_stream(sender, seq, samples);
            }
            Some(Datagram::Ping { t0, .. }) => {
                protocol::write_pong(&mut out, engine.me, t0, clock::now_us());
                let _ = engine.udp.send_to(&out, from);
            }
            Some(Datagram::Pong { sender, t0, t_remote }) => {
                let now = clock::now_us();
                let Some(peer) = engine.peers.read().get(&sender).cloned() else { continue };
                let rtt = now.saturating_sub(t0);
                let prev = peer.rtt_us.load(Ordering::Relaxed);
                let smoothed = if prev == 0 { rtt } else { (prev * 7 + rtt) / 8 };
                peer.rtt_us.store(smoothed, Ordering::Relaxed);
                peer.last_pong_us.store(now, Ordering::Relaxed);
                // Clock offset from the tightest samples only (least queuing);
                // the bar relaxes slowly so it can follow drift.
                let best = peer.best_rtt_us.load(Ordering::Relaxed);
                if rtt <= best.saturating_add(best / 4) {
                    let offset = t_remote as i64 - (t0 + rtt / 2) as i64;
                    peer.clock_offset_us.store(offset, Ordering::Relaxed);
                    peer.best_rtt_us.store(rtt.min(best), Ordering::Relaxed);
                } else {
                    peer.best_rtt_us.store(best + best / 100 + 1, Ordering::Relaxed);
                }
            }
            None => {}
        }
    }
}

fn ping(engine: Arc<Engine>) {
    let mut out = Vec::with_capacity(32);
    loop {
        thread::sleep(PING_INTERVAL);
        let now = clock::now_us();
        let peers: Vec<_> = engine.peers.read().values().cloned().collect();
        for p in peers {
            protocol::write_ping(&mut out, engine.me, now);
            let _ = engine.udp.send_to(&out, p.audio.udp);
            let silent_for = now.saturating_sub(p.last_pong_us.load(Ordering::Relaxed));
            if silent_for > DEAD_AFTER_US {
                // Closing the socket ends the reader, which unregisters.
                p.close();
            }
        }
    }
}

//! QUIC datagrams: audio receive, and ping/pong for RTT and clock offset.
//! (Liveness is QUIC's job: keep-alives, and a connection dies after six
//! silent seconds.)

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;

use crate::clock;
use crate::engine::{Engine, Peer};
use crate::protocol::{self, Datagram};

const PING_INTERVAL: Duration = Duration::from_millis(250);

pub fn spawn_pinger(engine: Arc<Engine>) {
    thread::Builder::new().name("ping".into()).spawn(move || ping(engine)).expect("spawn pinger");
}

/// Handles one peer's datagrams until its connection closes. The sender
/// field in each datagram is ignored: the connection says who it is.
pub async fn receive(engine: Arc<Engine>, peer: Arc<Peer>) {
    let mut out = Vec::with_capacity(64);
    let link = peer.link();
    while let Ok(bytes) = peer.conn.read_datagram().await {
        match Datagram::parse(&bytes) {
            Some(Datagram::Audio { seq, flags, samples, .. }) => {
                engine.audio.receive(peer.id, seq, flags, samples);
            }
            Some(Datagram::Stream { seq, samples, .. }) => {
                engine.audio.receive_stream(peer.id, seq, samples);
            }
            Some(Datagram::Ping { t0 }) => {
                protocol::write_pong(&mut out, engine.me, t0, clock::now_us());
                link.send(&out);
            }
            Some(Datagram::Pong { t0, t_remote, .. }) => {
                let now = clock::now_us();
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
            p.link().send(&out);
        }
    }
}

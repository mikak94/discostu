//! LAN discovery: periodic UDP broadcast beacons on a well-known port, and
//! manually added `host[:port]` peers. Beacons carry the group tag, so only
//! peers with the same group code dial each other.
//!
//! To avoid duplicate connections the peer with the lower id dials; the
//! other side dials too if nothing arrives within a few seconds (e.g. when
//! broadcast only works in one direction).

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};

use crate::engine::{Discovered, Engine};
use crate::net::{local_networks, quic};
use crate::protocol::{self, Beacon, DISCOVERY_PORT};

const BEACON_INTERVAL: Duration = Duration::from_millis(1000);
const MANUAL_RETRY: Duration = Duration::from_secs(5);
const FALLBACK_DIAL: Duration = Duration::from_secs(3);

fn socket() -> std::io::Result<UdpSocket> {
    let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    s.set_reuse_address(true)?;
    #[cfg(all(unix, not(target_os = "solaris")))]
    let _ = s.set_reuse_port(true);
    s.set_broadcast(true)?;
    s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)).into())?;
    s.set_read_timeout(Some(Duration::from_millis(200)))?;
    Ok(s.into())
}

pub fn spawn(engine: Arc<Engine>) {
    // Test instances: no beacons in or out, only `manual_peers`, so they
    // never show up in anyone's real session.
    let isolated = std::env::var_os("DISCOSTU_ISOLATED").is_some();
    thread::Builder::new()
        .name("discovery".into())
        .spawn(move || match if isolated { Err(std::io::Error::other("DISCOSTU_ISOLATED")) } else { socket() } {
            Ok(sock) => run(engine, sock),
            Err(e) => {
                eprintln!("broadcast discovery disabled: {e}");
                loop {
                    dial_manual(&engine);
                    thread::sleep(MANUAL_RETRY);
                }
            }
        })
        .expect("spawn discovery");
}

fn beacon(engine: &Engine) -> Vec<u8> {
    protocol::encode(&Beacon {
        magic: protocol::MAGIC,
        version: protocol::VERSION,
        id: engine.me,
        name: engine.name(),
        port: engine.net.port,
        group: engine.group().tag,
    })
}

fn run(engine: Arc<Engine>, sock: UdpSocket) {
    let mut last_beacon = Instant::now() - BEACON_INTERVAL;
    let mut last_manual = Instant::now() - MANUAL_RETRY;
    let mut buf = [0u8; 1500];
    loop {
        if last_beacon.elapsed() >= BEACON_INTERVAL {
            last_beacon = Instant::now();
            let b = beacon(&engine);
            let _ = sock.send_to(&b, (Ipv4Addr::BROADCAST, DISCOVERY_PORT));
            for net in local_networks() {
                let _ = sock.send_to(&b, (net.broadcast, DISCOVERY_PORT));
            }
        }
        if last_manual.elapsed() >= MANUAL_RETRY {
            last_manual = Instant::now();
            dial_manual(&engine);
        }
        if let Ok((n, from)) = sock.recv_from(&mut buf)
            && let Some(b) = protocol::decode::<Beacon>(&buf[..n])
        {
            on_beacon(&engine, b, from.ip());
        }
    }
}

/// `host[:port]` → addresses (DNS names resolve here).
pub fn resolve(target: &str) -> Vec<SocketAddr> {
    let target = target.trim();
    // "1.2.3.4:5", "host:5" and "[::1]:5" carry a port; "1.2.3.4", "host"
    // and "::1" don't.
    let has_port =
        target.parse::<SocketAddr>().is_ok() || target.rsplit_once(':').is_some_and(|(h, _)| !h.contains(':'));
    let with_port = if has_port {
        target.to_string()
    } else if target.contains(':') {
        format!("[{target}]:{}", protocol::DEFAULT_PORT)
    } else {
        format!("{target}:{}", protocol::DEFAULT_PORT)
    };
    with_port.to_socket_addrs().map(|a| a.collect()).unwrap_or_default()
}

fn dial_manual(engine: &Arc<Engine>) {
    for target in engine.manual_peers() {
        let addrs = resolve(&target);
        if !addrs.iter().any(|a| engine.is_connected_to(*a)) {
            quic::dial(engine, addrs, None);
        }
    }
}

fn on_beacon(engine: &Arc<Engine>, b: Beacon, ip: IpAddr) {
    if b.magic != protocol::MAGIC || b.version != protocol::VERSION || b.id == engine.me {
        return;
    }
    if b.group != engine.group().tag {
        return; // someone else's group
    }
    let now = Instant::now();
    let first_seen = {
        let mut d = engine.discovered.lock();
        let entry = d.entry(b.id).or_insert(Discovered { ip, port: b.port, first_seen: now, last_seen: now });
        entry.ip = ip;
        entry.port = b.port;
        entry.last_seen = now;
        entry.first_seen
    };
    if engine.peers.read().contains_key(&b.id) {
        return;
    }
    let should_dial = engine.me < b.id || now.duration_since(first_seen) > FALLBACK_DIAL;
    if should_dial {
        quic::dial(engine, vec![SocketAddr::new(ip, b.port)], Some(b.id));
    }
}

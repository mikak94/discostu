//! discostu broker: where friends groups meet.
//!
//! Each app keeps one QUIC connection here. The broker tells every member of
//! a group who else is online and at which addresses (including the public
//! address it sees them at, which is what lets two apps behind home routers
//! punch through to each other), and it keeps the group's channels. Audio and
//! video never pass through: peers connect to each other directly, or not
//! at all.
//!
//! Configuration (environment):
//! - `BROKER_BIND`: listen address, default `0.0.0.0:47900`. On Fly.io UDP
//!   must bind `fly-global-services:47900`.
//! - `BROKER_DATA`: directory for the identity key and the channel list,
//!   default `./broker-data`.

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use discostu_proto::broker::{
    ChannelEntry, FromBroker, MAX_ADDRS, MAX_CHANNELS, MAX_MEMBERS, MAX_MESSAGE, MAX_NAME, Member, Presence, ToBroker,
};
use discostu_proto::identity::{Group, GroupId, Identity};
use discostu_proto::tls;
use discostu_proto::wire::{self, Channel, PeerId};
use tokio::sync::mpsc;

const MAX_CONNECTIONS: usize = 2000;
/// Messages per connection per 10 s before it's dropped as abusive.
const MAX_RATE: u32 = 200;

struct Online {
    presence: Presence,
    observed: SocketAddr,
    tx: mpsc::UnboundedSender<FromBroker>,
    serial: u64,
    conn: quinn::Connection,
}

#[derive(Default)]
struct GroupState {
    members: HashMap<PeerId, Online>,
    channels: Vec<ChannelEntry>,
}

struct Broker {
    groups: HashMap<GroupId, GroupState>,
    store: PathBuf,
    next_serial: u64,
    connections: usize,
}

impl Broker {
    fn load(store: PathBuf) -> Self {
        let saved: HashMap<String, Vec<ChannelEntry>> = std::fs::read(&store)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let groups = saved
            .into_iter()
            .filter_map(|(hex, channels)| {
                Some((from_hex(&hex)?, GroupState { members: HashMap::new(), channels }))
            })
            .collect();
        Self { groups, store, next_serial: 1, connections: 0 }
    }

    fn save(&self) {
        let saved: HashMap<String, &Vec<ChannelEntry>> = self
            .groups
            .iter()
            .filter(|(_, g)| !g.channels.is_empty())
            .map(|(id, g)| (to_hex(id), &g.channels))
            .collect();
        let tmp = self.store.with_extension("tmp");
        match serde_json::to_vec_pretty(&saved) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&tmp, json).and_then(|_| std::fs::rename(&tmp, &self.store)) {
                    eprintln!("saving channels failed: {e}");
                }
            }
            Err(e) => eprintln!("encoding channels failed: {e}"),
        }
    }

    /// Sends the group's current state to all its members.
    fn broadcast(&mut self, group: &GroupId) {
        let Some(g) = self.groups.get(group) else { return };
        let members: Vec<Member> = g
            .members
            .iter()
            .map(|(id, m)| {
                let mut addrs = vec![m.observed];
                addrs.extend(m.presence.addrs.iter().filter(|a| **a != m.observed));
                Member { id: *id, name: m.presence.name.clone(), addrs, channel: m.presence.channel }
            })
            .collect();
        // Every channel, owner online or not: a sleeping PC mustn't empty a
        // channel everyone else is talking in.
        let state = FromBroker::State { members, channels: g.channels.clone() };
        for m in g.members.values() {
            let _ = m.tx.send(state.clone());
        }
    }

    fn handle(&mut self, group: &GroupId, me: PeerId, msg: ToBroker) {
        let Some(g) = self.groups.get_mut(group) else { return };
        let reply = |g: &GroupState, text: &str| {
            if let Some(m) = g.members.get(&me) {
                let _ = m.tx.send(FromBroker::Error(text.into()));
            }
        };
        match msg {
            ToBroker::Join { .. } => return,
            ToBroker::Presence(p) => {
                if let Some(m) = g.members.get_mut(&me) {
                    m.presence = sanitize(p);
                }
            }
            ToBroker::CreateChannel(c) => {
                let name = clip(&c.name);
                if name.is_empty() {
                    return reply(g, "a channel needs a name");
                }
                if g.channels.len() >= MAX_CHANNELS {
                    return reply(g, "this group has too many channels");
                }
                if g.channels.iter().any(|e| e.owner == me && e.channel.id == c.id) {
                    return;
                }
                g.channels.push(ChannelEntry { owner: me, channel: Channel { id: c.id, name } });
                self.save();
            }
            ToBroker::DeleteChannel(id) => {
                let before = g.channels.len();
                g.channels.retain(|e| !(e.owner == me && e.channel.id == id));
                if g.channels.len() == before {
                    return;
                }
                self.save();
            }
        }
        self.broadcast(group);
    }
}

fn clip(name: &str) -> String {
    name.trim().chars().filter(|c| !c.is_control()).take(MAX_NAME).collect()
}

fn sanitize(mut p: Presence) -> Presence {
    p.name = clip(&p.name);
    if p.name.is_empty() {
        p.name = "stu".into();
    }
    p.addrs.retain(|a| a.port() != 0 && !a.ip().is_unspecified() && !a.ip().is_multicast());
    p.addrs.truncate(MAX_ADDRS);
    p
}

/// A dual-stack socket reports IPv4 peers as `::ffff:a.b.c.d`.
fn unmap(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => a,
        },
        v4 => v4,
    }
}

fn to_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn from_hex(s: &str) -> Option<GroupId> {
    let bytes: Option<Vec<u8>> =
        (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect();
    bytes?.try_into().ok()
}

async fn serve(broker: Arc<Mutex<Broker>>, incoming: quinn::Incoming, open_group: GroupId) {
    let Ok(conn) = incoming.await else { return };
    let Some(me) = tls::remote_id(&conn) else { return };
    let observed = unmap(conn.remote_address());
    let Ok(Ok((mut send, mut recv))) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await else {
        return;
    };
    let Some(ToBroker::Join { group, presence }) = wire::recv_msg(&mut recv, MAX_MESSAGE).await else { return };
    if group == open_group {
        let _ = wire::send_msg(&mut send, &FromBroker::Error("set a group code to use the internet".into())).await;
        let _ = send.finish();
        let _ = tokio::time::timeout(Duration::from_secs(2), conn.closed()).await;
        return;
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let _ = tx.send(FromBroker::Welcome { observed });
    let serial = {
        let mut b = broker.lock().expect("broker lock");
        if b.connections >= MAX_CONNECTIONS {
            conn.close(1u32.into(), b"broker full");
            return;
        }
        let serial = b.next_serial;
        b.next_serial += 1;
        let g = b.groups.entry(group).or_default();
        if g.members.len() >= MAX_MEMBERS && !g.members.contains_key(&me) {
            conn.close(1u32.into(), b"group full");
            return;
        }
        let online = Online { presence: sanitize(presence), observed, tx, serial, conn: conn.clone() };
        if let Some(old) = g.members.insert(me, online) {
            // Same identity reconnected (new network, restart): the old
            // connection is stale.
            old.conn.close(0u32.into(), b"replaced");
        }
        b.connections += 1;
        b.broadcast(&group);
        serial
    };
    println!("+ {:016x} in {} from {observed}", me, &to_hex(&group)[..8]);

    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if wire::send_msg(&mut send, &msg).await.is_err() {
                break;
            }
        }
    });
    let mut window = (Instant::now(), 0u32);
    while let Some(msg) = wire::recv_msg::<ToBroker>(&mut recv, MAX_MESSAGE).await {
        if window.0.elapsed() > Duration::from_secs(10) {
            window = (Instant::now(), 0);
        }
        window.1 += 1;
        if window.1 > MAX_RATE {
            conn.close(2u32.into(), b"too many messages");
            break;
        }
        broker.lock().expect("broker lock").handle(&group, me, msg);
    }
    writer.abort();

    let mut b = broker.lock().expect("broker lock");
    b.connections -= 1;
    if let Some(g) = b.groups.get_mut(&group) {
        if g.members.get(&me).is_some_and(|m| m.serial == serial) {
            g.members.remove(&me);
            println!("- {:016x} in {}", me, &to_hex(&group)[..8]);
        }
        if g.members.is_empty() && g.channels.is_empty() {
            b.groups.remove(&group);
        } else {
            b.broadcast(&group);
        }
    }
}

fn bind_addr() -> SocketAddr {
    let spec = std::env::var("BROKER_BIND").unwrap_or_else(|_| format!("0.0.0.0:{}", tls::DEFAULT_BROKER_PORT));
    let addrs: Vec<SocketAddr> = spec
        .to_socket_addrs()
        .unwrap_or_else(|e| panic!("BROKER_BIND {spec}: {e}"))
        .collect();
    addrs.iter().find(|a| a.is_ipv4()).or(addrs.first()).copied().unwrap_or_else(|| panic!("BROKER_BIND {spec}: no address"))
}

#[tokio::main]
async fn main() {
    let data = PathBuf::from(std::env::var("BROKER_DATA").unwrap_or_else(|_| "broker-data".into()));
    std::fs::create_dir_all(&data).expect("data directory");
    let identity = Identity::load_or_create(&data.join("identity.bin")).expect("broker identity");
    let addr = bind_addr();
    let mut config = tls::server_config(&identity, tls::BROKER_ALPN.to_vec(), tls::broker_transport());
    config.max_incoming(MAX_CONNECTIONS);
    let endpoint = quinn::Endpoint::server(config, addr).expect("bind broker socket");
    println!("discostu broker on {addr}, fingerprint {}", identity.fingerprint());

    let broker = Arc::new(Mutex::new(Broker::load(store_path(&data))));
    let open_group = Group::from_code("").id;
    loop {
        tokio::select! {
            incoming = endpoint.accept() => match incoming {
                Some(i) => { tokio::spawn(serve(broker.clone(), i, open_group)); }
                None => break,
            },
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    println!("shutting down");
    endpoint.close(0u32.into(), b"broker restarting");
    let _ = tokio::time::timeout(Duration::from_secs(2), endpoint.wait_idle()).await;
}

fn store_path(data: &Path) -> PathBuf {
    data.join("channels.json")
}

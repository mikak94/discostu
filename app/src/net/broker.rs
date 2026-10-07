//! Broker client: one QUIC connection from the same endpoint (so the broker
//! sees the same public address and port our peers will use), reconnecting
//! with backoff. It learns who else in the group is online and dials them;
//! it keeps the group's channel list current.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use discostu_proto::broker::{FromBroker, MAX_MESSAGE, ToBroker};
use discostu_proto::identity::{self, Group};
use discostu_proto::tls;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{quic, rt};
use crate::engine::{BrokerStatus, Engine};
use crate::protocol as wire;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REDIAL: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub fn spawn(engine: Arc<Engine>) {
    rt().spawn(run(engine));
}

async fn run(engine: Arc<Engine>) {
    let mut backoff = Duration::from_secs(1);
    loop {
        let Some((group, target)) = engine.broker_target() else {
            engine.set_broker_status(BrokerStatus::Off);
            engine.broker_changed.notified().await;
            continue;
        };
        engine.set_broker_status(BrokerStatus::Connecting);
        match session(&engine, &group, &target).await {
            Ok(()) => backoff = Duration::from_secs(1),
            Err(e) => {
                engine.set_broker_status(BrokerStatus::Failed(e));
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = engine.broker_changed.notified() => {}
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
        engine.on_broker_lost();
    }
}

async fn resolve(engine: &Engine, target: &str) -> Result<SocketAddr, String> {
    let target = target.trim();
    let with_port = if target.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok()) {
        target.to_string()
    } else {
        format!("{target}:{}", tls::DEFAULT_BROKER_PORT)
    };
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host(&with_port)
        .await
        .map_err(|e| format!("can't find {target}: {e}"))?
        .collect();
    // IPv4 first: hosted brokers (Fly.io) take UDP on IPv4 only.
    addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.iter().find(|_| engine.net.ipv6))
        .copied()
        .ok_or_else(|| format!("{target} has no usable address"))
}

/// One connected session; `Ok` when it ended because settings changed.
async fn session(engine: &Arc<Engine>, group: &Group, target: &str) -> Result<(), String> {
    let addr = resolve(engine, target).await?;
    let connecting = engine
        .net
        .endpoint
        .connect_with(engine.net.broker_client.clone(), addr, tls::SERVER_NAME)
        .map_err(|e| e.to_string())?;
    let conn = timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| format!("no answer from {target}"))?
        .map_err(|e| e.to_string())?;

    // Trust on first use: the broker's certificate is self-signed, so we
    // remember its fingerprint and refuse a different one later.
    let cert = tls::remote_cert(&conn).ok_or("broker sent no certificate")?;
    let fingerprint = identity::fingerprint(&cert);
    match engine.broker_pin(target) {
        Some(pin) if pin != fingerprint => {
            conn.close(0u32.into(), b"unknown broker");
            return Err("the broker's identity changed. If you redeployed it, forget its key in Settings.".into());
        }
        Some(_) => {}
        None => engine.set_broker_pin(target, fingerprint),
    }

    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    wire::send_msg(&mut send, &ToBroker::Join { group: group.id, presence: engine.presence() })
        .await
        .map_err(|e| e.to_string())?;
    let (tx, mut outgoing) = mpsc::unbounded_channel();
    *engine.broker_tx.lock() = Some(tx.clone());
    for c in engine.take_legacy_channels() {
        let _ = tx.send(ToBroker::CreateChannel(c));
    }

    // Reads are not cancel-safe, so they get a task of their own.
    let (in_tx, mut incoming) = mpsc::unbounded_channel();
    let reader = tokio::spawn(async move {
        while let Some(msg) = wire::recv_msg::<FromBroker>(&mut recv, MAX_MESSAGE).await {
            if in_tx.send(msg).is_err() {
                break;
            }
        }
    });

    let mut redial = tokio::time::interval(REDIAL);
    let result = loop {
        tokio::select! {
            msg = incoming.recv() => match msg {
                Some(FromBroker::Welcome { observed }) => {
                    engine.set_broker_status(BrokerStatus::Connected { observed: super::unmap(observed) });
                }
                Some(FromBroker::State { members, channels }) => {
                    engine.on_broker_state(members, channels);
                    dial_members(engine);
                }
                Some(FromBroker::Error(e)) => engine.notify(format!("Broker: {e}")),
                None => {
                    break Err(match conn.close_reason() {
                        Some(quinn::ConnectionError::ApplicationClosed(c)) => {
                            format!("broker closed the connection: {}", String::from_utf8_lossy(&c.reason))
                        }
                        Some(e) => format!("lost the broker: {e}"),
                        None => "lost the broker".into(),
                    });
                }
            },
            Some(msg) = outgoing.recv() => {
                if let Err(e) = wire::send_msg(&mut send, &msg).await {
                    break Err(format!("lost the broker: {e}"));
                }
            }
            _ = redial.tick() => dial_members(engine),
            _ = engine.broker_changed.notified() => break Ok(()),
        }
    };
    *engine.broker_tx.lock() = None;
    reader.abort();
    conn.close(0u32.into(), b"bye");
    result
}

/// Dials every group member we aren't connected to, at all its addresses.
/// They dial us at the same time (same broker update), which is what gets
/// both routers to let the other side in.
fn dial_members(engine: &Arc<Engine>) {
    let members = engine.broker_members();
    for m in members {
        if m.id == engine.me || engine.peers.read().contains_key(&m.id) {
            continue;
        }
        let addrs: Vec<SocketAddr> = m.addrs.into_iter().filter(|a| a.is_ipv4() || engine.net.ipv6).collect();
        quic::dial(engine, addrs, Some(m.id));
    }
}

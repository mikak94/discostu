//! The QUIC endpoint: dialing, accepting, the handshake, and the per-peer
//! control stream.
//!
//! One connection per peer carries the control stream and the audio
//! datagrams. Watching a screen opens a second connection to the same
//! address, so video has its own congestion control and can't delay voice.
//!
//! Dialing tries every known address of a peer at once and keeps the first
//! that answers. When both sides do that at the same moment (the broker
//! tells them about each other together), each side's outgoing packets open
//! its own router for the other's: that's the hole punch. If both
//! connections complete, both sides keep the one the lower id opened.

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use discostu_proto::identity::Identity;
use discostu_proto::tls;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::time::timeout;

use super::{media, rt, unmap};
use crate::engine::Engine;
use crate::protocol::{self as wire, Ctrl, Hello, PeerId, StreamKind};

const MAX_CTRL: usize = 64 * 1024;
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Net {
    pub endpoint: quinn::Endpoint,
    pub port: u16,
    /// The socket is dual-stack (IPv6 candidates are worth sending).
    pub ipv6: bool,
    peer_client: quinn::ClientConfig,
    pub broker_client: quinn::ClientConfig,
}

impl Net {
    pub fn start(identity: &Identity) -> io::Result<Self> {
        let socket = super::bind_udp()?;
        let local = socket.local_addr()?;
        let server = tls::server_config(identity, tls::peer_alpn(), tls::peer_transport());
        let _rt = rt().enter();
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            Some(server),
            socket,
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok(Self {
            endpoint,
            port: local.port(),
            ipv6: local.is_ipv6(),
            peer_client: tls::client_config(identity, tls::peer_alpn(), tls::peer_transport()),
            broker_client: tls::client_config(identity, tls::BROKER_ALPN.to_vec(), tls::broker_transport()),
        })
    }

    /// Starts a connection attempt to each address; the first to finish its
    /// handshake wins and the others are abandoned.
    async fn connect_any(&self, addrs: &[SocketAddr]) -> Option<quinn::Connection> {
        let attempts: Vec<_> = addrs
            .iter()
            .filter_map(|a| self.endpoint.connect_with(self.peer_client.clone(), *a, tls::SERVER_NAME).ok())
            .map(Box::pin)
            .collect();
        if attempts.is_empty() {
            return None;
        }
        let (conn, _rest) = timeout(DIAL_TIMEOUT, futures::future::select_ok(attempts)).await.ok()?.ok()?;
        Some(conn)
    }
}

pub fn spawn_accept(engine: Arc<Engine>) {
    rt().spawn(async move {
        while let Some(incoming) = engine.net.endpoint.accept().await {
            tokio::spawn(accept(engine.clone(), incoming));
        }
    });
}

fn hello(engine: &Engine, conn: &quinn::Connection, kind: StreamKind) -> Option<Hello> {
    let secret = tls::session_secret(conn)?;
    Some(Hello { name: engine.name(), kind, proof: engine.group().proof(&secret, engine.me) })
}

/// Is the other side in our group? (It proved it knows the code.)
fn admitted(engine: &Engine, conn: &quinn::Connection, id: PeerId, hello: &Hello) -> bool {
    tls::session_secret(conn).is_some_and(|s| engine.group().verify(&s, id, &hello.proof))
}

fn refuse(conn: &quinn::Connection) {
    conn.close(3u32.into(), b"not in your group");
}

async fn accept(engine: Arc<Engine>, incoming: quinn::Incoming) -> Option<()> {
    let conn = incoming.await.ok()?;
    let id = tls::remote_id(&conn)?;
    if id == engine.me {
        conn.close(0u32.into(), b"that's me");
        return None;
    }
    let (mut send, mut recv) = timeout(HELLO_TIMEOUT, conn.accept_bi()).await.ok()?.ok()?;
    let theirs: Hello = timeout(HELLO_TIMEOUT, wire::recv_msg(&mut recv, MAX_CTRL)).await.ok()??;
    if !admitted(&engine, &conn, id, &theirs) {
        refuse(&conn);
        return None;
    }
    match theirs.kind {
        StreamKind::Screen => {
            // A peer wants to watch our screen; its audio goes over the
            // peer's main connection as datagrams.
            let Some(hub) = engine.share_hub() else {
                conn.close(4u32.into(), b"not sharing");
                return None;
            };
            let link = engine.peers.read().get(&id).map(|p| p.link());
            wire::send_msg(&mut send, &hello(&engine, &conn, StreamKind::Screen)?).await.ok()?;
            let writer = BlockingSend::new(send);
            std::thread::Builder::new()
                .name("screen-serve".into())
                .spawn(move || {
                    hub.serve(writer, link);
                    conn.close(0u32.into(), b"done");
                })
                .ok()?;
        }
        StreamKind::Control => {
            wire::send_msg(&mut send, &hello(&engine, &conn, StreamKind::Control)?).await.ok()?;
            run_peer(engine, conn, id, theirs, send, recv, id).await;
        }
    }
    Some(())
}

/// Connects to whichever of `addrs` answers first. `expected` is the peer's
/// id when known, which keeps one attempt per peer in flight.
pub fn dial(engine: &Arc<Engine>, addrs: Vec<SocketAddr>, expected: Option<PeerId>) {
    if addrs.is_empty() {
        return;
    }
    if let Some(id) = expected
        && (engine.peers.read().contains_key(&id) || !engine.connecting.lock().insert(id))
    {
        return;
    }
    let engine = engine.clone();
    rt().spawn(async move {
        if let Some(conn) = engine.net.connect_any(&addrs).await {
            dialed(engine.clone(), conn).await;
        }
        if let Some(id) = expected {
            engine.connecting.lock().remove(&id);
        }
    });
}

async fn dialed(engine: Arc<Engine>, conn: quinn::Connection) -> Option<()> {
    let id = tls::remote_id(&conn)?;
    if id == engine.me {
        conn.close(0u32.into(), b"that's me");
        return None;
    }
    let (mut send, mut recv) = conn.open_bi().await.ok()?;
    wire::send_msg(&mut send, &hello(&engine, &conn, StreamKind::Control)?).await.ok()?;
    let theirs: Hello = timeout(HELLO_TIMEOUT, wire::recv_msg(&mut recv, MAX_CTRL)).await.ok()??;
    if theirs.kind != StreamKind::Control || !admitted(&engine, &conn, id, &theirs) {
        refuse(&conn);
        return None;
    }
    let me = engine.me;
    // Registration happens inside; the connecting mark must be cleared
    // before this future ends, which `dial` does.
    run_peer(engine, conn, id, theirs, send, recv, me).await;
    Some(())
}

/// Registers the peer and serves its connection until it closes.
async fn run_peer(
    engine: Arc<Engine>,
    conn: quinn::Connection,
    id: PeerId,
    hello: Hello,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    initiator: PeerId,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Ctrl>();
    let Some(peer) = engine.register(id, &hello.name, conn.clone(), tx, initiator) else {
        conn.close(0u32.into(), b"duplicate");
        return;
    };
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let bye = matches!(msg, Ctrl::Bye);
            if wire::send_msg(&mut send, &msg).await.is_err() || bye {
                break;
            }
        }
    });
    let datagrams = tokio::spawn(media::receive(engine.clone(), peer.clone()));
    while let Some(msg) = wire::recv_msg::<Ctrl>(&mut recv, MAX_CTRL).await {
        match msg {
            Ctrl::Status(status) => engine.on_status(id, status),
            Ctrl::Bye => break,
        }
    }
    writer.abort();
    datagrams.abort();
    conn.close(0u32.into(), b"bye");
    engine.unregister(id, peer.serial);
}

/// Opens a screen-watching connection to a peer we're already connected to.
pub async fn open_screen(
    engine: Arc<Engine>,
    addr: SocketAddr,
    expected: PeerId,
) -> Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream), String> {
    let conn = engine
        .net
        .endpoint
        .connect_with(engine.net.peer_client.clone(), addr, tls::SERVER_NAME)
        .map_err(|e| e.to_string())?;
    let conn = timeout(DIAL_TIMEOUT, conn).await.map_err(|_| "timed out".to_string())?.map_err(|e| e.to_string())?;
    if tls::remote_id(&conn) != Some(expected) {
        conn.close(0u32.into(), b"wrong peer");
        return Err("a different peer answered".into());
    }
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    let ours = hello(&engine, &conn, StreamKind::Screen).ok_or("no session secret")?;
    wire::send_msg(&mut send, &ours).await.map_err(|e| e.to_string())?;
    let theirs: Hello = timeout(HELLO_TIMEOUT, wire::recv_msg(&mut recv, MAX_CTRL))
        .await
        .ok()
        .flatten()
        .ok_or("they're not sharing any more")?;
    if !admitted(&engine, &conn, expected, &theirs) {
        refuse(&conn);
        return Err("not in your group".into());
    }
    Ok((conn, send, recv))
}

pub fn remote_addr(conn: &quinn::Connection) -> SocketAddr {
    unmap(conn.remote_address())
}

/// A QUIC send stream for blocking code (the screen-share writer threads).
pub struct BlockingSend {
    stream: quinn::SendStream,
    handle: Handle,
}

impl BlockingSend {
    pub fn new(stream: quinn::SendStream) -> Self {
        Self { stream, handle: rt().handle().clone() }
    }

    pub fn close(&mut self) {
        let _ = self.stream.finish();
    }
}

impl Write for BlockingSend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.handle.block_on(self.stream.write(buf)).map_err(io::Error::other)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A QUIC receive stream for blocking code (the screen-share reader thread).
pub struct BlockingRecv {
    stream: quinn::RecvStream,
    handle: Handle,
}

impl BlockingRecv {
    pub fn new(stream: quinn::RecvStream) -> Self {
        Self { stream, handle: rt().handle().clone() }
    }
}

impl Read for BlockingRecv {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self.handle.block_on(self.stream.read(buf)) {
            Ok(Some(n)) => Ok(n),
            Ok(None) => Ok(0),
            Err(e) => Err(io::Error::other(e)),
        }
    }
}

//! TCP: accept loop, handshake, and the per-peer control reader.

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::engine::Engine;
use crate::protocol::{self, Ctrl, Hello, PeerId, StreamKind};

const MAX_CTRL: usize = 64 * 1024;

pub fn spawn_listener(engine: Arc<Engine>, listener: TcpListener) {
    thread::Builder::new()
        .name("tcp-listen".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let e = engine.clone();
                let _ = thread::Builder::new().name("tcp-conn".into()).spawn(move || accept(e, stream));
            }
        })
        .expect("spawn listener");
}

fn read_hello(stream: &mut TcpStream, me: PeerId) -> Option<Hello> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut buf = Vec::new();
    protocol::read_frame(stream, &mut buf, MAX_CTRL).ok()?;
    let _ = stream.set_read_timeout(None);
    let hello: Hello = protocol::decode(&buf)?;
    (hello.magic == protocol::MAGIC && hello.version == protocol::VERSION && hello.id != me)
        .then_some(hello)
}

fn accept(engine: Arc<Engine>, mut stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(peer_addr) = stream.peer_addr() else { return };
    let Some(hello) = read_hello(&mut stream, engine.me) else { return };
    match hello.kind {
        StreamKind::Screen => {
            // A peer wants to watch our screen.
            if let Some(hub) = engine.share_hub() {
                // Screen audio goes to the viewer's media socket.
                let audio_to = engine.peers.read().get(&hello.id).map(|p| p.audio.udp);
                hub.serve(stream, audio_to);
            }
        }
        StreamKind::Control => {
            let ours = protocol::encode(&engine.hello(StreamKind::Control));
            if protocol::write_frame(&mut stream, &ours).is_ok() {
                let initiator = hello.id;
                run(engine, stream, hello, peer_addr, initiator);
            }
        }
    }
}

/// Opens a control connection. `expected` is the peer id if known (from a
/// beacon); it's cleared from the connecting set on failure.
pub fn connect(engine: Arc<Engine>, addr: SocketAddr, expected: Option<PeerId>) {
    let _ = thread::Builder::new().name("tcp-connect".into()).spawn(move || {
        let attempt = || {
            let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
            let _ = stream.set_nodelay(true);
            let ours = protocol::encode(&engine.hello(StreamKind::Control));
            protocol::write_frame(&mut stream, &ours).ok()?;
            let hello = read_hello(&mut stream, engine.me)?;
            Some((stream, hello))
        };
        match attempt() {
            Some((stream, hello)) => {
                let me = engine.me;
                run(engine, stream, hello, addr, me)
            }
            None => {
                if let Some(id) = expected {
                    engine.connecting.lock().remove(&id);
                }
            }
        }
    });
}

/// Registers the peer and reads control messages until the socket closes.
fn run(engine: Arc<Engine>, mut stream: TcpStream, hello: Hello, addr: SocketAddr, initiator: PeerId) {
    let Some(peer) = engine.register(&hello, addr.ip(), &stream, initiator) else {
        engine.connecting.lock().remove(&hello.id);
        return; // lost duplicate resolution; dropping closes the socket
    };
    let mut buf = Vec::new();
    while protocol::read_frame(&mut stream, &mut buf, MAX_CTRL).is_ok() {
        match protocol::decode::<Ctrl>(&buf) {
            Some(Ctrl::Status(status)) => engine.on_status(peer.id, status),
            Some(Ctrl::Bye) | None => break,
        }
    }
    engine.unregister(peer.id, peer.serial);
}

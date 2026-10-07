//! Networking. Everything runs over QUIC on one UDP port:
//!
//! - `quic`: the endpoint, dialing (several candidate addresses at once,
//!   which is also how NAT hole punching happens), the handshake with group
//!   proofs, and the per-peer control stream.
//! - `media`: voice/stream-audio datagrams and clock pings.
//! - `discovery`: LAN broadcast beacons and manual `host[:port]` peers.
//! - `broker`: the internet meeting point (presence, addresses, channels).
//! - `portmap`: asks the router for a port mapping (UPnP) so others can
//!   reach us even when hole punching alone wouldn't.

pub mod broker;
pub mod discovery;
pub mod media;
pub mod portmap;
pub mod quic;

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::sync::OnceLock;

use bytes::Bytes;
use socket2::{Domain, Protocol, Socket, Type};

use crate::protocol::DEFAULT_PORT;

/// The tokio runtime the network runs on (iced has its own).
pub fn rt() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("net")
            .on_thread_start(|| {
                // Voice datagrams are handled on these threads.
                let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
            })
            .enable_all()
            .build()
            .expect("network runtime")
    })
}

/// Runs `fut` to completion from synchronous code, whatever thread that is
/// (the UI thread may already be inside another runtime's context).
pub fn block_on<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> T {
    let handle = rt().handle().clone();
    std::thread::spawn(move || handle.block_on(fut)).join().expect("network task panicked")
}

/// Sends datagrams to one peer over its QUIC connection.
#[derive(Clone)]
pub struct Link(pub quinn::Connection);

impl Link {
    /// Unreliable, unordered, encrypted. `false` once the connection is gone.
    pub fn send(&self, packet: &[u8]) -> bool {
        self.0.send_datagram(Bytes::copy_from_slice(packet)).is_ok()
    }
}

/// The UDP socket every connection shares: dual-stack when IPv6 exists, on
/// the well-known port unless another instance holds it.
pub fn bind_udp() -> io::Result<UdpSocket> {
    let make = |v6: bool, port: u16| -> io::Result<UdpSocket> {
        let s = if v6 {
            let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_only_v6(false)?;
            exclusive(&s)?;
            s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
            s
        } else {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            // Expedited-forwarding DSCP so APs/switches that honour it prioritise audio.
            let _ = s.set_tos_v4(0xB8);
            exclusive(&s)?;
            s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
            s
        };
        let _ = s.set_recv_buffer_size(4 << 20);
        let _ = s.set_send_buffer_size(4 << 20);
        Ok(s.into())
    };
    let socket = make(true, DEFAULT_PORT)
        .or_else(|_| make(false, DEFAULT_PORT))
        .or_else(|_| make(true, 0))
        .or_else(|_| make(false, 0))?;
    disable_connreset(&socket);
    Ok(socket)
}

/// Windows otherwise lets a second wildcard socket share the port (a
/// dual-stack one next to an IPv4 one, for instance), and packets then land
/// in whichever socket Windows picks: another discostu would steal ours.
#[cfg(windows)]
fn exclusive(socket: &Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    const SOL_SOCKET: i32 = 0xffff;
    const SO_EXCLUSIVEADDRUSE: i32 = !4; // ~SO_REUSEADDR
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn setsockopt(s: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
    }
    let on: u32 = 1;
    let r = unsafe {
        setsockopt(socket.as_raw_socket() as usize, SOL_SOCKET, SO_EXCLUSIVEADDRUSE, (&on as *const u32).cast(), 4)
    };
    if r == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(windows))]
fn exclusive(_: &Socket) -> io::Result<()> {
    Ok(())
}

/// Windows reports ICMP "port unreachable" from an earlier send as an error
/// on the *next* recv; turn that off so a departed peer can't disturb us.
#[cfg(windows)]
fn disable_connreset(socket: &UdpSocket) {
    use std::os::windows::io::AsRawSocket;
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;
    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAIoctl(
            s: usize,
            code: u32,
            inbuf: *const u32,
            inlen: u32,
            outbuf: *mut u8,
            outlen: u32,
            returned: *mut u32,
            overlapped: *mut u8,
            completion: *mut u8,
        ) -> i32;
    }
    let off: u32 = 0;
    let mut returned = 0u32;
    unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            &off,
            4,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(windows))]
fn disable_connreset(_: &UdpSocket) {}

pub struct LocalNet {
    pub ip: Ipv4Addr,
    pub broadcast: Ipv4Addr,
}

pub fn local_networks() -> Vec<LocalNet> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback() && i.is_oper_up())
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) if !v4.ip.is_link_local() => {
                let broadcast = v4
                    .broadcast
                    .unwrap_or_else(|| Ipv4Addr::from(u32::from(v4.ip) | !u32::from(v4.netmask)));
                Some(LocalNet { ip: v4.ip, broadcast })
            }
            _ => None,
        })
        .collect()
}

pub fn local_ipv4s() -> Vec<IpAddr> {
    local_networks().into_iter().map(|n| IpAddr::V4(n.ip)).collect()
}

/// Globally routable IPv6 addresses of this machine (no NAT in the way, only
/// firewalls, which simultaneous dialing opens).
pub fn global_ipv6s() -> Vec<IpAddr> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter(|i| !i.is_loopback() && i.is_oper_up())
        .filter_map(|i| match i.addr {
            // 2000::/3 is global unicast.
            if_addrs::IfAddr::V6(v6) if (v6.ip.segments()[0] & 0xe000) == 0x2000 => Some(IpAddr::V6(v6.ip)),
            _ => None,
        })
        .collect()
}

/// Private, CGNAT, loopback or link-local: not reachable from the internet.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || v4.is_loopback() || v4.is_link_local() || (o[0] == 100 && (o[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback() || (v6.segments()[0] & 0xfe00) == 0xfc00 || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

/// A dual-stack socket reports IPv4 peers as `::ffff:a.b.c.d`.
pub fn unmap(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(v4.into(), v6.port()),
            None => a,
        },
        v4 => v4,
    }
}

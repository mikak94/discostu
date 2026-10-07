//! Peer-to-peer networking: LAN discovery, control/screen TCP, media UDP.

pub mod control;
pub mod discovery;
pub mod media;

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};

use socket2::{Domain, Protocol, Socket, Type};

use crate::protocol::{DEFAULT_TCP_PORT, DEFAULT_UDP_PORT};

/// Binds the well-known TCP port, or an ephemeral one if another instance
/// already holds it (beacons advertise the real port either way).
pub fn bind_tcp() -> io::Result<(TcpListener, u16)> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, DEFAULT_TCP_PORT))
        .or_else(|_| TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)))?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

pub fn bind_udp() -> io::Result<(UdpSocket, u16)> {
    let make = |port: u16| -> io::Result<UdpSocket> {
        let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        let _ = s.set_recv_buffer_size(4 << 20);
        let _ = s.set_send_buffer_size(4 << 20);
        // Expedited-forwarding DSCP so APs/switches that honour it prioritise audio.
        let _ = s.set_tos_v4(0xB8);
        s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
        Ok(s.into())
    };
    let socket = make(DEFAULT_UDP_PORT).or_else(|_| make(0))?;
    disable_connreset(&socket);
    let port = socket.local_addr()?.port();
    Ok((socket, port))
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

//! Router port mapping: asks the home router to forward our UDP port, so
//! peers can reach us directly even when hole punching alone would fail
//! (both sides behind strict NATs). Tries UPnP IGD first, then PCP and its
//! predecessor NAT-PMP (Apple-style routers, many ISPs). The lease is renewed
//! while we run and removed on exit; nothing stays open afterwards.
//!
//! Useless behind carrier-grade NAT (the router's "external" address is
//! itself private), which is detected and reported instead of advertised.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use crab_nat::{InternetProtocol, PortMapping, PortMappingOptions, PortMappingType, TimeoutConfig};
use igd_next::aio::Gateway;
use igd_next::aio::tokio::{Tokio, search_gateway};
use igd_next::{PortMappingProtocol, SearchOptions};
use parking_lot::Mutex;

use super::{block_on, is_private, rt};
use crate::engine::Engine;

const LEASE: Duration = Duration::from_secs(3600);
const RENEW: Duration = Duration::from_secs(1800);
const RETRY: Duration = Duration::from_secs(600);
const DESCRIPTION: &str = "discostu";

#[derive(Debug, Clone, Default, PartialEq)]
pub enum PortMap {
    #[default]
    Trying,
    /// Open at this public address; `via` is the protocol that did it.
    Mapped { addr: SocketAddr, via: &'static str },
    /// The router answered but its external address is private.
    CarrierNat(IpAddr),
    Unavailable(String),
    Disabled,
}

enum Active {
    Upnp(Gateway<Tokio>, u16),
    Pcp(PortMapping),
}

/// What to remove on exit.
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

pub fn spawn(engine: Arc<Engine>) {
    let isolated = std::env::var_os("DISCOSTU_ISOLATED").is_some() && std::env::var_os("DISCOSTU_UPNP").is_none();
    if isolated || std::env::var_os("DISCOSTU_NO_UPNP").is_some() {
        engine.set_portmap(PortMap::Disabled);
        return;
    }
    rt().spawn(async move {
        loop {
            let state = match upnp(engine.net.port).await {
                Ok(s) => s,
                Err(upnp_error) => match pcp(engine.net.port).await {
                    Ok(s) => s,
                    Err(pcp_error) => PortMap::Unavailable(format!("UPnP: {upnp_error}; PCP/NAT-PMP: {pcp_error}")),
                },
            };
            let ok = matches!(state, PortMap::Mapped { .. });
            engine.set_portmap(state);
            tokio::time::sleep(if ok { RENEW } else { RETRY }).await;
        }
    });
}

async fn upnp(port: u16) -> Result<PortMap, String> {
    let options = SearchOptions { timeout: Some(Duration::from_secs(3)), ..Default::default() };
    let gateway = search_gateway(options).await.map_err(|_| "no router answered".to_string())?;
    let external = gateway.get_external_ip().await.map_err(|e| e.to_string())?;
    if is_private(external) {
        return Ok(PortMap::CarrierNat(external));
    }
    let local = local_addr_towards(gateway.addr, port).ok_or("no route to the router")?;
    let lease = LEASE.as_secs() as u32;
    let mapped = match gateway.add_port(PortMappingProtocol::UDP, port, local, lease, DESCRIPTION).await {
        Ok(()) => port,
        // Someone else holds that external port: take any.
        Err(_) => gateway
            .add_any_port(PortMappingProtocol::UDP, local, lease, DESCRIPTION)
            .await
            .map_err(|e| format!("router refused ({e})"))?,
    };
    *ACTIVE.lock() = Some(Active::Upnp(gateway, mapped));
    Ok(PortMap::Mapped { addr: SocketAddr::new(external, mapped), via: "UPnP" })
}

async fn pcp(port: u16) -> Result<PortMap, String> {
    let gateway = netdev::get_default_gateway().map_err(|e| format!("no default gateway ({e})"))?;
    let gw = *gateway.ipv4.first().ok_or("no IPv4 gateway")?;
    let local = local_addr_towards(SocketAddr::new(gw.into(), 5351), port).ok_or("no route to the router")?;
    let internal = NonZeroU16::new(port).ok_or("no port")?;
    let options = PortMappingOptions {
        external_port: Some(internal),
        lifetime_seconds: Some(LEASE.as_secs() as u32),
        timeout_config: Some(TimeoutConfig {
            initial_timeout: Duration::from_millis(250),
            max_retries: 2,
            max_retry_timeout: Some(Duration::from_secs(1)),
        }),
    };
    let mapping = PortMapping::new(gw.into(), local.ip(), InternetProtocol::Udp, internal, options)
        .await
        .map_err(|e| e.to_string())?;
    let external: IpAddr = match mapping.mapping_type() {
        PortMappingType::Pcp { external_ip, .. } => external_ip,
        PortMappingType::NatPmp => crab_nat::natpmp::external_address(gw.into(), None)
            .await
            .map_err(|e| e.to_string())?
            .into(),
    };
    let via = match mapping.mapping_type() {
        PortMappingType::Pcp { .. } => "PCP",
        PortMappingType::NatPmp => "NAT-PMP",
    };
    let addr = SocketAddr::new(external, mapping.external_port().get());
    *ACTIVE.lock() = Some(Active::Pcp(mapping));
    if is_private(external) {
        return Ok(PortMap::CarrierNat(external));
    }
    Ok(PortMap::Mapped { addr, via })
}

/// Our LAN address on the router's network (connecting a UDP socket sends
/// nothing; it just picks the route).
fn local_addr_towards(gateway: SocketAddr, port: u16) -> Option<SocketAddr> {
    let probe = UdpSocket::bind("0.0.0.0:0").ok()?;
    probe.connect(gateway).ok()?;
    Some(SocketAddr::new(probe.local_addr().ok()?.ip(), port))
}

/// Removes our mapping (at exit). Gives up after a second.
pub fn release() {
    let Some(active) = ACTIVE.lock().take() else { return };
    block_on(async move {
        let remove = async {
            match active {
                Active::Upnp(gateway, port) => {
                    let _ = gateway.remove_port(PortMappingProtocol::UDP, port).await;
                }
                Active::Pcp(mapping) => {
                    let _ = mapping.try_drop().await;
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), remove).await;
    });
}

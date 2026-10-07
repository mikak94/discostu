//! App ↔ broker messages.
//!
//! The broker is a meeting point, not a relay: it tells the members of a
//! friends group where each other can be reached, and keeps the group's
//! channels. One QUIC connection per app; the app opens one bidirectional
//! stream and sends [`ToBroker::Join`] first; every later change on either
//! side is a framed message on that stream.

use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::identity::GroupId;
use crate::wire::{Channel, ChannelId, ChannelRef, PeerId};

/// Largest message either side accepts.
pub const MAX_MESSAGE: usize = 64 * 1024;
pub const MAX_NAME: usize = 32;
pub const MAX_ADDRS: usize = 16;
pub const MAX_CHANNELS: usize = 64;
pub const MAX_MEMBERS: usize = 64;

/// What a member tells the group about itself.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Presence {
    pub name: String,
    /// Where we might be reachable besides the address the broker sees:
    /// LAN addresses (same network, no hairpin needed), global IPv6, and a
    /// router port mapping (UPnP).
    pub addrs: Vec<SocketAddr>,
    pub channel: Option<ChannelRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToBroker {
    Join { group: GroupId, presence: Presence },
    Presence(Presence),
    /// Ids are chosen by the creator (random); a duplicate is ignored.
    CreateChannel(Channel),
    /// Only the owner can delete.
    DeleteChannel(ChannelId),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Member {
    pub id: PeerId,
    pub name: String,
    /// The address the broker sees first, then the member's own list.
    pub addrs: Vec<SocketAddr>,
    pub channel: Option<ChannelRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelEntry {
    pub owner: PeerId,
    pub channel: Channel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FromBroker {
    /// After [`ToBroker::Join`]: the address the broker sees us at.
    Welcome { observed: SocketAddr },
    /// The whole group, sent again whenever anything in it changes.
    /// Channels are listed only while their owner is online.
    State { members: Vec<Member>, channels: Vec<ChannelEntry> },
    /// Something was refused; the connection stays up.
    Error(String),
}

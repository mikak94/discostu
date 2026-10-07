//! The backend the UI talks to: owns peers, audio, and screen sharing.
//!
//! UI calls are cheap, non-blocking methods; the UI reads state by polling
//! [`Engine::snapshot`] (high-rate data like levels lives in atomics, so a
//! snapshot is just a few loads).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use discostu_proto::broker::{ChannelEntry, Member, Presence, ToBroker};
use discostu_proto::identity::{self, Group, Identity};
use parking_lot::{Mutex, RwLock};
use tokio::sync::mpsc;

use crate::audio::jitter::JitterStats;
use crate::audio::{AudioSettings, AudioShared, DeviceSpec, DeviceStatus, Driver, NetStats, PeerAudio};
use crate::config::Config;
use crate::net::{self, Link, portmap::PortMap, quic};
use crate::protocol::{Channel, ChannelId, ChannelRef, Ctrl, PeerId, PeerStatus, ShareInfo};
use crate::screen::{Hub, HubStats, Source, VideoSink, VideoStats, Viewer};

/// A channel we created stays listed this long before the broker confirms it.
const PENDING_CHANNEL: Duration = Duration::from_secs(10);

pub struct Peer {
    pub id: PeerId,
    /// Where their packets come from (LAN, public, or mapped address).
    pub addr: SocketAddr,
    pub status: Mutex<PeerStatus>,
    pub conn: quinn::Connection,
    ctrl: mpsc::UnboundedSender<Ctrl>,
    /// Which side opened this connection (for duplicate resolution).
    pub initiator: PeerId,
    pub serial: u64,
    pub audio: Arc<PeerAudio>,
    pub rtt_us: AtomicU64,
    pub best_rtt_us: AtomicU64,
    /// Their clock minus ours, from the lowest-RTT ping samples.
    pub clock_offset_us: Arc<AtomicI64>,
    pub last_pong_us: AtomicU64,
}

impl Peer {
    pub fn send(&self, msg: &Ctrl) -> bool {
        self.ctrl.send(msg.clone()).is_ok()
    }

    pub fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
    }

    pub fn link(&self) -> Link {
        Link(self.conn.clone())
    }
}

pub struct Discovered {
    pub ip: IpAddr,
    pub port: u16,
    pub first_seen: Instant,
    pub last_seen: Instant,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum BrokerStatus {
    /// No group code, or no broker configured.
    #[default]
    Off,
    Connecting,
    /// `observed`: our public address as the broker sees it.
    Connected { observed: SocketAddr },
    Failed(String),
}

/// The running engine, for a clean shutdown when the window closes.
static RUNNING: Mutex<Option<std::sync::Weak<Engine>>> = Mutex::new(None);

/// Shuts the running engine down (peers told, router mapping removed).
pub fn shutdown_running() {
    if let Some(e) = RUNNING.lock().take().and_then(|w| w.upgrade()) {
        e.shutdown();
    }
}

pub struct Engine {
    pub me: PeerId,
    pub net: quic::Net,
    pub audio: Arc<AudioShared>,
    cfg: Mutex<Config>,
    group: RwLock<Group>,
    pub peers: RwLock<HashMap<PeerId, Arc<Peer>>>,
    pub connecting: Mutex<HashSet<PeerId>>,
    pub discovered: Mutex<HashMap<PeerId, Discovered>>,
    share: Mutex<Option<Hub>>,
    watching: Mutex<Option<(PeerId, Viewer)>>,
    serials: AtomicU64,
    muted: AtomicBool,
    deafened: AtomicBool,
    /// The channel we're in; `None` = the lobby.
    channel: Mutex<Option<ChannelRef>>,
    /// One-shot message for the UI (e.g. "channel closed").
    notice: Mutex<Option<String>>,
    /// Wakes the broker client when the group or broker address changes.
    pub broker_changed: tokio::sync::Notify,
    /// Messages to the broker while connected.
    pub broker_tx: Mutex<Option<mpsc::UnboundedSender<ToBroker>>>,
    broker_status: Mutex<BrokerStatus>,
    /// Group members online at the broker (us included).
    members: Mutex<Vec<Member>>,
    /// The group's open channels, as last heard from the broker (kept while
    /// it's briefly unreachable).
    channels: Mutex<Vec<ChannelEntry>>,
    /// Ours, created but not yet in a broker update.
    pending_channels: Mutex<Vec<(Channel, Instant)>>,
    portmap: Mutex<PortMap>,
}

#[derive(Debug, Clone, Default)]
pub struct MeView {
    pub id: PeerId,
    pub name: String,
    pub muted: bool,
    pub deafened: bool,
    pub level: f32,
    pub speaking: bool,
    pub sharing: Option<ShareInfo>,
    pub share: Option<HubStats>,
    pub profile_progress: f32,
    pub echo_delay_ms: Option<f32>,
}

#[derive(Debug, Clone)]
pub struct PeerView {
    pub id: PeerId,
    pub name: String,
    pub addr: SocketAddr,
    /// "LAN" or "internet".
    pub path: &'static str,
    pub muted: bool,
    pub deafened: bool,
    pub sharing: Option<ShareInfo>,
    pub rtt_us: u64,
    pub speaking: bool,
    pub colocated: bool,
    pub volume: f32,
    pub jitter: JitterStats,
    pub net: NetStats,
    pub stream_jitter: JitterStats,
    pub stream_net: NetStats,
    /// Their screen-share audio is arriving.
    pub streaming: bool,
    pub channel: Option<ChannelRef>,
}

#[derive(Debug, Clone)]
pub struct ChannelView {
    pub at: ChannelRef,
    pub name: String,
    pub host: String,
    pub mine: bool,
}

/// Group members the broker says are online but we have no connection to
/// (yet, or at all: both behind strict NATs).
#[derive(Debug, Clone)]
pub struct WaitingView {
    pub name: String,
}

#[derive(Debug, Clone, Default)]
pub struct InternetView {
    /// `XXXX-XXXX-…`, empty without a group.
    pub group_code: String,
    pub broker: String,
    pub status: BrokerStatus,
    pub portmap: PortMap,
    pub waiting: Vec<WaitingView>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub me: MeView,
    pub peers: Vec<PeerView>,
    /// Our channel; `None` = the lobby.
    pub channel: Option<ChannelRef>,
    /// Every open channel: ours first, then by host.
    pub channels: Vec<ChannelView>,
    pub internet: InternetView,
    pub devices: DeviceStatus,
    pub watching: Option<PeerId>,
    pub video: Option<VideoStats>,
    pub addresses: Vec<String>,
    pub playback_underruns: u64,
    pub send_errors: u64,
    pub echo_cancel: bool,
    pub crosstalk_cancel: bool,
    pub noise_gate: bool,
    pub share_audio: bool,
    pub stream_volume: f32,
    pub dsp_load: f32,
}

fn profile_file(base: &str, ext: &str) -> String {
    match std::env::var("DISCOSTU_PROFILE") {
        Ok(p) if !p.is_empty() => format!("{base}-{p}.{ext}"),
        _ => format!("{base}.{ext}"),
    }
}

impl Engine {
    pub fn start() -> Result<Arc<Self>, String> {
        single_instance()?;
        let mut cfg = Config::load();
        if cfg.audio_driver.is_none() {
            // First run: prefer a real ASIO driver when one is installed.
            cfg.audio_driver = Some(match preferred_asio_driver() {
                Some(d) => {
                    cfg.input_device = Some(d.clone());
                    cfg.output_device = Some(d);
                    Driver::Asio
                }
                None => Driver::Wasapi,
            });
            cfg.save();
        }
        let identity = Identity::load_or_create(&Config::dir().join(profile_file("identity", "bin")))
            .map_err(|e| format!("identity: {e}"))?;
        let net = quic::Net::start(&identity).map_err(|e| format!("network: {e}"))?;
        let audio = AudioShared::start(
            identity.id,
            AudioSettings {
                spec: device_spec(&cfg),
                stream_volume: cfg.stream_volume,
                echo_cancel: cfg.echo_cancel,
                crosstalk_cancel: cfg.crosstalk_cancel,
                noise_gate: cfg.noise_gate,
                profile_path: Config::dir().join(profile_file("voice-profile", "json")),
            },
        );
        let group = Group::from_code(&cfg.group_code);
        let engine = Arc::new(Self {
            me: identity.id,
            net,
            audio,
            cfg: Mutex::new(cfg),
            group: RwLock::new(group),
            peers: RwLock::new(HashMap::new()),
            connecting: Mutex::new(HashSet::new()),
            discovered: Mutex::new(HashMap::new()),
            share: Mutex::new(None),
            watching: Mutex::new(None),
            serials: AtomicU64::new(1),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            channel: Mutex::new(None),
            notice: Mutex::new(None),
            broker_changed: tokio::sync::Notify::new(),
            broker_tx: Mutex::new(None),
            broker_status: Mutex::new(BrokerStatus::Off),
            members: Mutex::new(Vec::new()),
            channels: Mutex::new(Vec::new()),
            pending_channels: Mutex::new(Vec::new()),
            portmap: Mutex::new(PortMap::Trying),
        });
        quic::spawn_accept(engine.clone());
        net::media::spawn_pinger(engine.clone());
        net::discovery::spawn(engine.clone());
        net::broker::spawn(engine.clone());
        net::portmap::spawn(engine.clone());
        *RUNNING.lock() = Some(Arc::downgrade(&engine));
        Ok(engine)
    }

    /// Leaves cleanly: tells peers, removes the router port mapping.
    pub fn shutdown(&self) {
        for p in self.peers.read().values() {
            p.send(&Ctrl::Bye);
        }
        net::portmap::release();
        self.net.endpoint.close(0u32.into(), b"bye");
    }

    pub fn name(&self) -> String {
        self.cfg.lock().name.clone()
    }

    pub fn group(&self) -> Group {
        self.group.read().clone()
    }

    pub fn manual_peers(&self) -> Vec<String> {
        self.cfg.lock().manual_peers.clone()
    }

    pub fn local_status(&self) -> PeerStatus {
        PeerStatus {
            name: self.name(),
            muted: self.muted.load(Ordering::Relaxed),
            deafened: self.deafened.load(Ordering::Relaxed),
            sharing: self.share.lock().as_ref().filter(|h| !h.is_stopped()).map(|h| h.info()),
            channel: *self.channel.lock(),
        }
    }

    fn broadcast_status(&self) {
        let msg = Ctrl::Status(self.local_status());
        for p in self.peers.read().values() {
            p.send(&msg);
        }
        self.send_presence();
    }

    pub fn share_hub(&self) -> Option<Hub> {
        self.share.lock().clone().filter(|h| !h.is_stopped())
    }

    pub fn notify(&self, msg: String) {
        *self.notice.lock() = Some(msg);
    }

    // --- peer lifecycle (called from net tasks) ---------------------------

    /// Installs a handshaken connection. Returns the peer if this connection
    /// won duplicate resolution.
    pub fn register(
        &self,
        id: PeerId,
        name: &str,
        conn: quinn::Connection,
        ctrl: mpsc::UnboundedSender<Ctrl>,
        initiator: PeerId,
    ) -> Option<Arc<Peer>> {
        let preferred = self.me.min(id);
        let mut peers = self.peers.write();
        if let Some(existing) = peers.get(&id) {
            // Both sides apply the same rule, so they keep the same connection.
            if existing.initiator == preferred && initiator != preferred {
                return None;
            }
            existing.close();
        }
        let volume = self.cfg.lock().peer_volumes.get(&id).copied().unwrap_or(1.0);
        let audio = self.audio.add_peer(id, Link(conn.clone()), volume);
        let peer = Arc::new(Peer {
            id,
            addr: quic::remote_addr(&conn),
            status: Mutex::new(PeerStatus { name: name.to_string(), ..Default::default() }),
            conn,
            ctrl,
            initiator,
            serial: self.serials.fetch_add(1, Ordering::Relaxed),
            audio,
            rtt_us: AtomicU64::new(0),
            best_rtt_us: AtomicU64::new(u64::MAX),
            clock_offset_us: Arc::new(AtomicI64::new(0)),
            last_pong_us: AtomicU64::new(crate::clock::now_us()),
        });
        peers.insert(id, peer.clone());
        drop(peers);
        self.connecting.lock().remove(&id);
        peer.send(&Ctrl::Status(self.local_status()));
        self.reroute();
        Some(peer)
    }

    pub fn unregister(&self, id: PeerId, serial: u64) {
        let mut peers = self.peers.write();
        if peers.get(&id).is_some_and(|p| p.serial == serial) {
            if let Some(p) = peers.remove(&id) {
                p.close();
            }
            drop(peers);
            self.audio.remove_peer(id);
            self.reroute();
            let mut w = self.watching.lock();
            if w.as_ref().is_some_and(|(pid, _)| *pid == id) {
                *w = None;
            }
        }
    }

    pub fn on_status(&self, id: PeerId, status: PeerStatus) {
        let stopped_sharing = status.sharing.is_none();
        if let Some(p) = self.peers.read().get(&id) {
            *p.status.lock() = status;
        }
        self.reroute();
        if stopped_sharing {
            let mut w = self.watching.lock();
            if w.as_ref().is_some_and(|(pid, _)| *pid == id) {
                *w = None;
            }
        }
    }

    // --- broker (called from net::broker) ------------------------------------

    /// The group and broker to connect to; `None` without a group code or
    /// with the broker turned off.
    pub fn broker_target(&self) -> Option<(Group, String)> {
        let group = self.group();
        let broker = self.cfg.lock().broker.trim().to_string();
        (!group.is_open() && !broker.is_empty()).then_some((group, broker))
    }

    pub fn broker_pin(&self, broker: &str) -> Option<String> {
        self.cfg.lock().broker_pins.get(broker).cloned()
    }

    pub fn set_broker_pin(&self, broker: &str, fingerprint: String) {
        let mut cfg = self.cfg.lock();
        cfg.broker_pins.insert(broker.to_string(), fingerprint);
        cfg.save();
    }

    pub fn set_broker_status(&self, status: BrokerStatus) {
        *self.broker_status.lock() = status;
    }

    pub fn on_broker_state(&self, members: Vec<Member>, channels: Vec<ChannelEntry>) {
        self.pending_channels.lock().retain(|(c, at)| {
            at.elapsed() < PENDING_CHANNEL && !channels.iter().any(|e| e.owner == self.me && e.channel.id == c.id)
        });
        *self.members.lock() = members;
        *self.channels.lock() = channels;
        self.reroute();
    }

    pub fn on_broker_lost(&self) {
        self.members.lock().clear();
        *self.broker_tx.lock() = None;
    }

    pub fn broker_members(&self) -> Vec<Member> {
        self.members.lock().clone()
    }

    /// What the group should know about us.
    pub fn presence(&self) -> Presence {
        let port = self.net.port;
        let mut addrs: Vec<SocketAddr> = Vec::new();
        if let PortMap::Mapped { addr: a, .. } = *self.portmap.lock() {
            addrs.push(a);
        }
        addrs.extend(net::local_ipv4s().into_iter().map(|ip| SocketAddr::new(ip, port)));
        if self.net.ipv6 {
            addrs.extend(net::global_ipv6s().into_iter().map(|ip| SocketAddr::new(ip, port)));
        }
        Presence { name: self.name(), addrs, channel: *self.channel.lock() }
    }

    fn send_presence(&self) {
        if let Some(tx) = self.broker_tx.lock().as_ref() {
            let _ = tx.send(ToBroker::Presence(self.presence()));
        }
    }

    /// Channels from before they lived on the broker: handed over once.
    pub fn take_legacy_channels(&self) -> Vec<Channel> {
        let mut cfg = self.cfg.lock();
        let legacy = std::mem::take(&mut cfg.channels);
        if !legacy.is_empty() {
            cfg.save();
        }
        legacy
    }

    pub fn set_portmap(&self, state: PortMap) {
        let changed = {
            let mut p = self.portmap.lock();
            let changed = *p != state;
            *p = state;
            changed
        };
        if changed {
            self.send_presence();
        }
    }

    // --- UI actions ---------------------------------------------------------

    /// Joins the friends group with this code (empty: LAN only, no group).
    /// Drops current connections: they belong to the old group.
    pub fn set_group_code(&self, code: &str) {
        let group = Group::from_code(code);
        {
            let mut cfg = self.cfg.lock();
            if cfg.group_code == group.code {
                return;
            }
            cfg.group_code = group.code.clone();
            cfg.save();
        }
        *self.group.write() = group;
        *self.channel.lock() = None;
        self.channels.lock().clear();
        self.pending_channels.lock().clear();
        self.members.lock().clear();
        for p in self.peers.read().values() {
            p.close();
        }
        self.broker_changed.notify_one();
    }

    /// Starts a new group with a fresh code and returns it.
    pub fn new_group(&self) -> String {
        let code = identity::new_code();
        self.set_group_code(&code);
        code
    }

    pub fn set_broker(&self, address: &str) {
        {
            let mut cfg = self.cfg.lock();
            let address = address.trim();
            if cfg.broker == address {
                return;
            }
            cfg.broker = address.to_string();
            cfg.save();
        }
        self.broker_changed.notify_one();
    }

    /// Forgets the remembered broker key (after redeploying the broker).
    pub fn forget_broker_key(&self) {
        let mut cfg = self.cfg.lock();
        let broker = cfg.broker.trim().to_string();
        cfg.broker_pins.remove(&broker);
        cfg.save();
        drop(cfg);
        self.broker_changed.notify_one();
    }

    /// Moves us to `to` (`None` = the lobby) if that channel is open.
    pub fn join(&self, to: Option<ChannelRef>) -> Result<(), String> {
        if let Some(r) = to
            && !self.channel_open(r)
        {
            return Err("that channel just closed".into());
        }
        *self.channel.lock() = to;
        self.reroute();
        self.broadcast_status();
        Ok(())
    }

    /// Creates a channel (kept by the broker for the group) and moves us in.
    pub fn create_channel(&self, name: &str) -> Result<(), String> {
        let name: String = name.trim().chars().take(32).collect();
        if name.is_empty() {
            return Err("give it a name".into());
        }
        let channel = Channel { id: identity::random_u64(), name };
        {
            let tx = self.broker_tx.lock();
            let tx = tx.as_ref().ok_or("channels live on the broker: set a group code and get connected first")?;
            let _ = tx.send(ToBroker::CreateChannel(channel.clone()));
        }
        let id = channel.id;
        self.pending_channels.lock().push((channel, Instant::now()));
        self.join(Some(ChannelRef { owner: self.me, id }))
    }

    /// Deletes one of our channels for good; anyone in it drops to the lobby.
    pub fn delete_channel(&self, id: ChannelId) {
        if let Some(tx) = self.broker_tx.lock().as_ref() {
            let _ = tx.send(ToBroker::DeleteChannel(id));
        }
        self.channels.lock().retain(|e| !(e.owner == self.me && e.channel.id == id));
        self.pending_channels.lock().retain(|(c, _)| c.id != id);
        self.reroute();
        self.broadcast_status();
    }

    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().take()
    }

    /// Open = the broker lists it (its owner is online), or we just made it.
    fn channel_open(&self, r: ChannelRef) -> bool {
        if r.owner == self.me && self.pending_channels.lock().iter().any(|(c, _)| c.id == r.id) {
            return true;
        }
        self.channels.lock().iter().any(|e| e.owner == r.owner && e.channel.id == r.id)
    }

    /// Recomputes who we exchange voice with. If our channel closed (its
    /// owner left or deleted it), drops us back to the lobby.
    fn reroute(&self) {
        let current = *self.channel.lock();
        let mut here = current;
        if let Some(r) = current
            && !self.channel_open(r)
        {
            here = None;
            *self.channel.lock() = None;
            *self.notice.lock() = Some("The channel closed: its owner left. You're back in the lobby.".into());
        }
        for p in self.peers.read().values() {
            p.audio.set_in_channel(p.status.lock().channel == here);
        }
        if here != current {
            self.broadcast_status();
        }
    }

    fn name_of(&self, id: PeerId) -> String {
        if id == self.me {
            return self.name();
        }
        if let Some(p) = self.peers.read().get(&id) {
            return p.status.lock().name.clone();
        }
        self.members.lock().iter().find(|m| m.id == id).map_or_else(|| "someone".into(), |m| m.name.clone())
    }

    fn channel_views(&self) -> Vec<ChannelView> {
        let mut all: Vec<(PeerId, Channel)> =
            self.channels.lock().iter().map(|e| (e.owner, e.channel.clone())).collect();
        for (c, _) in self.pending_channels.lock().iter() {
            if !all.iter().any(|(o, x)| *o == self.me && x.id == c.id) {
                all.push((self.me, c.clone()));
            }
        }
        let mut out: Vec<ChannelView> = all
            .into_iter()
            .map(|(owner, c)| ChannelView {
                at: ChannelRef { owner, id: c.id },
                name: c.name,
                host: self.name_of(owner),
                mine: owner == self.me,
            })
            .collect();
        out.sort_by(|a, b| {
            (!a.mine, a.host.to_lowercase(), a.name.to_lowercase()).cmp(&(
                !b.mine,
                b.host.to_lowercase(),
                b.name.to_lowercase(),
            ))
        });
        out
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
        self.audio.muted.store(muted, Ordering::Relaxed);
        self.broadcast_status();
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.deafened.store(deafened, Ordering::Relaxed);
        self.audio.deafened.store(deafened, Ordering::Relaxed);
        self.broadcast_status();
    }

    pub fn set_name(&self, name: String) {
        let name = name.trim().chars().take(32).collect::<String>();
        if name.is_empty() {
            return;
        }
        let mut cfg = self.cfg.lock();
        cfg.name = name;
        cfg.save();
        drop(cfg);
        self.broadcast_status();
    }

    /// Starts sharing `source`. Returns a warning if it had to fall back.
    pub fn start_share(&self, source: &Source) -> Result<Option<String>, String> {
        self.stop_share();
        let audio = self.cfg.lock().share_audio;
        let (hub, warning) = Hub::start(source, audio, self.me)?;
        *self.share.lock() = Some(hub);
        self.broadcast_status();
        Ok(warning)
    }

    pub fn stop_share(&self) {
        if let Some(hub) = self.share.lock().take() {
            hub.stop();
            self.broadcast_status();
        }
    }

    /// Opens a second connection to the peer for their screen (blocking).
    pub fn watch(self: &Arc<Self>, id: PeerId) -> Result<(), String> {
        let peer = self.peers.read().get(&id).cloned().ok_or("peer is gone")?;
        let (conn, send, recv) = net::block_on(quic::open_screen(self.clone(), peer.addr, id))?;
        let viewer = Viewer::start(conn, send, recv, peer.clock_offset_us.clone())?;
        *self.watching.lock() = Some((id, viewer));
        Ok(())
    }

    pub fn unwatch(&self) {
        *self.watching.lock() = None;
    }

    pub fn video_sink(&self) -> Option<Arc<VideoSink>> {
        self.watching.lock().as_ref().map(|(_, v)| v.sink.clone())
    }

    pub fn set_volume(&self, id: PeerId, volume: f32) {
        if let Some(a) = self.audio.peer(id) {
            a.volume.store(volume);
        }
        let mut cfg = self.cfg.lock();
        cfg.peer_volumes.insert(id, volume);
        cfg.save();
    }

    pub fn audio_spec(&self) -> DeviceSpec {
        device_spec(&self.cfg.lock())
    }

    pub fn set_audio_spec(&self, spec: DeviceSpec) {
        let mut cfg = self.cfg.lock();
        cfg.audio_driver = Some(spec.driver);
        cfg.input_device = spec.input.clone();
        cfg.output_device = spec.output.clone();
        cfg.asio_buffer = spec.asio_buffer;
        cfg.mic_channel = spec.mic_channel;
        cfg.exclusive_audio = spec.exclusive;
        cfg.save();
        drop(cfg);
        self.audio.set_spec(spec);
    }

    pub fn set_share_audio(&self, on: bool) {
        let mut cfg = self.cfg.lock();
        cfg.share_audio = on;
        cfg.save();
    }

    pub fn set_stream_volume(&self, v: f32) {
        self.audio.stream_volume.store(v);
        let mut cfg = self.cfg.lock();
        cfg.stream_volume = v;
        cfg.save();
    }

    pub fn set_toggle(&self, toggle: Toggle, on: bool) {
        let mut cfg = self.cfg.lock();
        let (field, atomic) = match toggle {
            Toggle::EchoCancel => (&mut cfg.echo_cancel, &self.audio.echo_cancel),
            Toggle::CrosstalkCancel => (&mut cfg.crosstalk_cancel, &self.audio.crosstalk_cancel),
            Toggle::NoiseGate => (&mut cfg.noise_gate, &self.audio.noise_gate),
        };
        *field = on;
        atomic.store(on, Ordering::Relaxed);
        cfg.save();
    }

    pub fn reset_voice_profile(&self) {
        self.audio.reset_voice_profile();
    }

    /// Saves the last 10 s of every audio stage for diagnosis.
    pub fn save_diagnostics(&self) {
        self.audio.request_snapshot();
    }

    /// The folder once the recording is written (adds who each peer is).
    pub fn take_diagnostics_result(&self) -> Option<Result<std::path::PathBuf, String>> {
        let result = self.audio.take_snapshot_result()?;
        if let Ok(dir) = &result {
            let names: String = self
                .peers
                .read()
                .values()
                .map(|p| format!("{:016x} {} {}\n", p.id, p.status.lock().name, p.addr))
                .collect();
            let _ = std::fs::write(dir.join("peers.txt"), names);
        }
        Some(result)
    }

    /// Connects to `host[:port]` and remembers it for automatic reconnects.
    pub fn connect_manual(self: &Arc<Self>, target: &str) -> Result<(), String> {
        let target = target.trim();
        let addrs = net::discovery::resolve(target);
        if addrs.is_empty() {
            return Err("could not resolve address".into());
        }
        {
            let mut cfg = self.cfg.lock();
            if !cfg.manual_peers.iter().any(|p| p == target) {
                cfg.manual_peers.push(target.to_string());
                cfg.save();
            }
        }
        quic::dial(self, addrs, None);
        Ok(())
    }

    pub fn is_connected_to(&self, addr: SocketAddr) -> bool {
        let addr = net::unmap(addr);
        self.peers.read().values().any(|p| p.addr == addr)
    }

    fn internet_view(&self) -> InternetView {
        let cfg = self.cfg.lock();
        let group_code = identity::pretty_code(&cfg.group_code);
        let broker = cfg.broker.clone();
        drop(cfg);
        let peers = self.peers.read();
        let waiting = self
            .members
            .lock()
            .iter()
            .filter(|m| m.id != self.me && !peers.contains_key(&m.id))
            .map(|m| WaitingView { name: m.name.clone() })
            .collect();
        InternetView {
            group_code,
            broker,
            status: self.broker_status.lock().clone(),
            portmap: self.portmap.lock().clone(),
            waiting,
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let a = &self.audio;
        let hub = self.share_hub();
        let echo_delay = a.echo_delay_ms.load();
        let me = MeView {
            id: self.me,
            name: self.name(),
            muted: self.muted.load(Ordering::Relaxed),
            deafened: self.deafened.load(Ordering::Relaxed),
            level: a.local_level.load(),
            speaking: a.local_voice.load(Ordering::Relaxed),
            sharing: hub.as_ref().map(|h| h.info()),
            share: hub.as_ref().map(|h| h.stats()),
            profile_progress: a.profile_progress.load(),
            echo_delay_ms: (echo_delay >= 0.0).then_some(echo_delay),
        };
        let now = crate::clock::now_us();
        let mut peers: Vec<PeerView> = self
            .peers
            .read()
            .values()
            .map(|p| {
                let st = p.status.lock().clone();
                let au = &p.audio;
                let fresh = now.saturating_sub(au.last_packet_us.load(Ordering::Relaxed)) < 200_000;
                PeerView {
                    id: p.id,
                    name: st.name,
                    addr: p.addr,
                    path: if net::is_private(p.addr.ip()) { "LAN" } else { "internet" },
                    muted: st.muted,
                    deafened: st.deafened,
                    sharing: st.sharing,
                    rtt_us: p.rtt_us.load(Ordering::Relaxed),
                    speaking: fresh && au.voice_active.load(Ordering::Relaxed),
                    colocated: au.colocated.load(Ordering::Relaxed),
                    volume: au.volume.load(),
                    jitter: *au.stats.lock(),
                    net: *au.net.lock(),
                    stream_jitter: *au.stream_stats.lock(),
                    stream_net: *au.stream_net.lock(),
                    streaming: au.streaming(),
                    channel: st.channel,
                }
            })
            .collect();
        peers.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));

        // Drop a viewer whose connection died.
        {
            let mut w = self.watching.lock();
            if w.as_ref().is_some_and(|(_, v)| !v.sink.connected.load(Ordering::Relaxed)) {
                *w = None;
            }
        }
        let (watching, video) = match self.watching.lock().as_ref() {
            Some((id, v)) => (Some(*id), Some(v.sink.stats.lock().expect("stats lock").clone())),
            None => (None, None),
        };
        let channels = self.channel_views();
        let internet = self.internet_view();
        let cfg = self.cfg.lock();
        Snapshot {
            me,
            peers,
            channel: *self.channel.lock(),
            channels,
            internet,
            devices: a.status.lock().clone(),
            watching,
            video,
            addresses: net::local_ipv4s().iter().map(|ip| format!("{ip}:{}", self.net.port)).collect(),
            playback_underruns: a.playback_underruns(),
            send_errors: a.send_errors(),
            echo_cancel: cfg.echo_cancel,
            crosstalk_cancel: cfg.crosstalk_cancel,
            noise_gate: cfg.noise_gate,
            share_audio: cfg.share_audio,
            stream_volume: cfg.stream_volume,
            dsp_load: a.dsp_load.load(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toggle {
    EchoCancel,
    CrosstalkCancel,
    NoiseGate,
}

fn device_spec(cfg: &Config) -> DeviceSpec {
    DeviceSpec {
        driver: cfg.audio_driver.unwrap_or_default(),
        input: cfg.input_device.clone(),
        output: cfg.output_device.clone(),
        asio_buffer: cfg.asio_buffer,
        mic_channel: cfg.mic_channel,
        exclusive: cfg.exclusive_audio,
    }
}

/// An installed ASIO driver for real hardware, skipping software wrappers. Wrappers
/// sit on top of WASAPI, so native WASAPI is never slower than they are.
fn preferred_asio_driver() -> Option<String> {
    if !crate::audio::device::asio_available() {
        return None;
    }
    let (drivers, _) = crate::audio::device::list(Driver::Asio);
    const WRAPPERS: [&str; 5] = ["fl studio", "asio4all", "flexasio", "generic", "realtek"];
    drivers
        .iter()
        .find(|d| !WRAPPERS.iter().any(|w| d.to_lowercase().contains(w)))
        .cloned()
}

/// One running copy per profile. Two with the same identity keep replacing
/// each other's connections at every peer, which looks like a flaky network.
#[cfg(windows)]
fn single_instance() -> Result<(), String> {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;
    let profile = std::env::var("DISCOSTU_PROFILE").unwrap_or_default();
    let name = HSTRING::from(format!("Local\\discostu-{profile}"));
    // Never closed: held until the process exits.
    let mutex = unsafe { CreateMutexW(None, false, &name) }.map_err(|e| e.to_string())?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err("discostu is already running with this profile. Close the other copy first.".into());
    }
    let _ = mutex;
    Ok(())
}

#[cfg(not(windows))]
fn single_instance() -> Result<(), String> {
    Ok(())
}

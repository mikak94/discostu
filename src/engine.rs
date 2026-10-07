//! The backend the UI talks to: owns peers, audio, and screen sharing.
//!
//! UI calls are cheap, non-blocking methods; the UI reads state by polling
//! [`Engine::snapshot`] (high-rate data like levels lives in atomics, so a
//! snapshot is just a few loads).

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use parking_lot::{Mutex, RwLock};

use crate::audio::jitter::JitterStats;
use crate::audio::{AudioSettings, AudioShared, DeviceSpec, DeviceStatus, Driver, NetStats, PeerAudio};
use crate::config::Config;
use crate::net;
use crate::protocol::{self, Channel, ChannelId, ChannelRef, Ctrl, Hello, PeerId, PeerStatus, ShareInfo, StreamKind};
use crate::screen::{Hub, HubStats, Source, VideoSink, VideoStats, Viewer};

pub struct Peer {
    pub id: PeerId,
    pub ip: IpAddr,
    pub tcp_port: u16,
    pub status: Mutex<PeerStatus>,
    pub writer: Mutex<TcpStream>,
    /// Which side opened this control connection (for duplicate resolution).
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
        let bytes = protocol::encode(msg);
        protocol::write_frame(&mut *self.writer.lock(), &bytes).is_ok()
    }

    pub fn close(&self) {
        let _ = self.writer.lock().shutdown(std::net::Shutdown::Both);
    }
}

pub struct Discovered {
    pub ip: IpAddr,
    pub tcp_port: u16,
    pub first_seen: Instant,
    pub last_seen: Instant,
}

pub struct Engine {
    pub me: PeerId,
    pub tcp_port: u16,
    pub udp_port: u16,
    pub udp: UdpSocket,
    pub audio: Arc<AudioShared>,
    cfg: Mutex<Config>,
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
    pub ip: IpAddr,
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

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub me: MeView,
    pub peers: Vec<PeerView>,
    /// Our channel; `None` = the lobby.
    pub channel: Option<ChannelRef>,
    /// Every open channel: ours first, then by host.
    pub channels: Vec<ChannelView>,
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
        let (listener, tcp_port) = net::bind_tcp().map_err(|e| format!("TCP: {e}"))?;
        let (udp, udp_port) = net::bind_udp().map_err(|e| format!("UDP: {e}"))?;
        let profile_name = match std::env::var("DISCOSTU_PROFILE") {
            Ok(p) if !p.is_empty() => format!("voice-profile-{p}.json"),
            _ => "voice-profile.json".into(),
        };
        let audio = AudioShared::start(
            cfg.id,
            udp.try_clone().map_err(|e| e.to_string())?,
            AudioSettings {
                spec: device_spec(&cfg),
                stream_volume: cfg.stream_volume,
                echo_cancel: cfg.echo_cancel,
                crosstalk_cancel: cfg.crosstalk_cancel,
                noise_gate: cfg.noise_gate,
                profile_path: Config::dir().join(profile_name),
            },
        );
        let engine = Arc::new(Self {
            me: cfg.id,
            tcp_port,
            udp_port,
            udp,
            audio,
            cfg: Mutex::new(cfg),
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
        });
        net::control::spawn_listener(engine.clone(), listener);
        net::media::spawn(engine.clone());
        net::discovery::spawn(engine.clone());
        Ok(engine)
    }

    pub fn name(&self) -> String {
        self.cfg.lock().name.clone()
    }

    pub fn manual_peers(&self) -> Vec<String> {
        self.cfg.lock().manual_peers.clone()
    }

    pub fn hello(&self, kind: StreamKind) -> Hello {
        Hello {
            magic: protocol::MAGIC,
            version: protocol::VERSION,
            id: self.me,
            name: self.name(),
            tcp_port: self.tcp_port,
            udp_port: self.udp_port,
            kind,
        }
    }

    pub fn local_status(&self) -> PeerStatus {
        PeerStatus {
            name: self.name(),
            muted: self.muted.load(Ordering::Relaxed),
            deafened: self.deafened.load(Ordering::Relaxed),
            sharing: self.share.lock().as_ref().filter(|h| !h.is_stopped()).map(|h| h.info()),
            hosting: self.cfg.lock().channels.clone(),
            channel: *self.channel.lock(),
        }
    }

    fn broadcast_status(&self) {
        let msg = Ctrl::Status(self.local_status());
        for p in self.peers.read().values() {
            p.send(&msg);
        }
    }

    pub fn share_hub(&self) -> Option<Hub> {
        self.share.lock().clone().filter(|h| !h.is_stopped())
    }

    // --- peer lifecycle (called from net threads) -------------------------

    /// Installs a handshaken control connection. Returns the peer if this
    /// connection won duplicate resolution.
    pub fn register(&self, hello: &Hello, ip: IpAddr, stream: &TcpStream, initiator: PeerId) -> Option<Arc<Peer>> {
        let writer = stream.try_clone().ok()?;
        let _ = writer.set_write_timeout(Some(std::time::Duration::from_secs(1)));
        let preferred = self.me.min(hello.id);
        let mut peers = self.peers.write();
        if let Some(existing) = peers.get(&hello.id) {
            // Both sides apply the same rule, so they keep the same socket.
            if existing.initiator == preferred && initiator != preferred {
                return None;
            }
            existing.close();
        }
        let volume = self.cfg.lock().peer_volumes.get(&hello.id).copied().unwrap_or(1.0);
        let audio = self.audio.add_peer(hello.id, SocketAddr::new(ip, hello.udp_port), volume);
        let peer = Arc::new(Peer {
            id: hello.id,
            ip,
            tcp_port: hello.tcp_port,
            status: Mutex::new(PeerStatus { name: hello.name.clone(), ..Default::default() }),
            writer: Mutex::new(writer),
            initiator,
            serial: self.serials.fetch_add(1, Ordering::Relaxed),
            audio,
            rtt_us: AtomicU64::new(0),
            best_rtt_us: AtomicU64::new(u64::MAX),
            clock_offset_us: Arc::new(AtomicI64::new(0)),
            last_pong_us: AtomicU64::new(crate::clock::now_us()),
        });
        peers.insert(hello.id, peer.clone());
        drop(peers);
        self.connecting.lock().remove(&hello.id);
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

    // --- UI actions ---------------------------------------------------------

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

    /// Creates a channel we host, saves it, and moves us into it.
    pub fn create_channel(&self, name: &str) -> Result<(), String> {
        let name: String = name.trim().chars().take(32).collect();
        if name.is_empty() {
            return Err("give it a name".into());
        }
        let id = crate::config::random_id();
        let mut cfg = self.cfg.lock();
        cfg.channels.push(Channel { id, name });
        cfg.save();
        drop(cfg);
        self.join(Some(ChannelRef { owner: self.me, id }))
    }

    /// Deletes one of our channels for good; anyone in it drops to the lobby.
    pub fn delete_channel(&self, id: ChannelId) {
        let mut cfg = self.cfg.lock();
        cfg.channels.retain(|c| c.id != id);
        cfg.save();
        drop(cfg);
        self.reroute();
        self.broadcast_status();
    }

    pub fn take_notice(&self) -> Option<String> {
        self.notice.lock().take()
    }

    /// Open = its host is online (us, or a connected peer announcing it).
    fn channel_open(&self, r: ChannelRef) -> bool {
        if r.owner == self.me {
            return self.cfg.lock().channels.iter().any(|c| c.id == r.id);
        }
        self.peers
            .read()
            .get(&r.owner)
            .is_some_and(|p| p.status.lock().hosting.iter().any(|c| c.id == r.id))
    }

    /// Recomputes who we exchange voice with. If our channel closed (its host
    /// left or deleted it), drops us back to the lobby.
    fn reroute(&self) {
        let current = *self.channel.lock();
        let mut here = current;
        if let Some(r) = current
            && !self.channel_open(r)
        {
            here = None;
            *self.channel.lock() = None;
            *self.notice.lock() = Some("The channel closed: its host left. You're back in the lobby.".into());
        }
        for p in self.peers.read().values() {
            p.audio.set_in_channel(p.status.lock().channel == here);
        }
        if here != current {
            self.broadcast_status();
        }
    }

    fn channel_views(&self) -> Vec<ChannelView> {
        let me = self.name();
        let mut out: Vec<ChannelView> = self
            .cfg
            .lock()
            .channels
            .iter()
            .map(|c| ChannelView {
                at: ChannelRef { owner: self.me, id: c.id },
                name: c.name.clone(),
                host: me.clone(),
                mine: true,
            })
            .collect();
        let mut theirs = Vec::new();
        for p in self.peers.read().values() {
            let st = p.status.lock();
            theirs.extend(st.hosting.iter().map(|c| ChannelView {
                at: ChannelRef { owner: p.id, id: c.id },
                name: c.name.clone(),
                host: st.name.clone(),
                mine: false,
            }));
        }
        theirs.sort_by(|a, b| {
            (a.host.to_lowercase(), a.name.to_lowercase()).cmp(&(b.host.to_lowercase(), b.name.to_lowercase()))
        });
        out.extend(theirs);
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
        let (hub, warning) = Hub::start(source, audio, self.me, &self.udp)?;
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

    pub fn watch(&self, id: PeerId) -> Result<(), String> {
        let peer = self.peers.read().get(&id).cloned().ok_or("peer is gone")?;
        let viewer = Viewer::connect(
            SocketAddr::new(peer.ip, peer.tcp_port),
            self.hello(StreamKind::Screen),
            peer.clock_offset_us.clone(),
        )?;
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
                .map(|p| format!("{:016x} {}
", p.id, p.status.lock().name))
                .collect();
            let _ = std::fs::write(dir.join("peers.txt"), names);
        }
        Some(result)
    }

    /// Connects to `host[:port]` and remembers it for automatic reconnects.
    pub fn connect_manual(self: &Arc<Self>, target: &str) -> Result<(), String> {
        let target = target.trim();
        let with_port =
            if target.contains(':') { target.to_string() } else { format!("{target}:{}", protocol::DEFAULT_TCP_PORT) };
        let addr = with_port
            .to_socket_addrs()
            .map_err(|e| e.to_string())?
            .find(|a| a.is_ipv4())
            .ok_or("could not resolve address")?;
        {
            let mut cfg = self.cfg.lock();
            if !cfg.manual_peers.iter().any(|p| p == target) {
                cfg.manual_peers.push(target.to_string());
                cfg.save();
            }
        }
        net::control::connect(self.clone(), addr, None);
        Ok(())
    }

    pub fn is_connected_to(&self, addr: SocketAddr) -> bool {
        self.peers.read().values().any(|p| p.ip == addr.ip() && p.tcp_port == addr.port())
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
                    ip: p.ip,
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
        let cfg = self.cfg.lock();
        Snapshot {
            me,
            peers,
            channel: *self.channel.lock(),
            channels,
            devices: a.status.lock().clone(),
            watching,
            video,
            addresses: net::local_ipv4s().iter().map(|ip| format!("{ip}:{}", self.tcp_port)).collect(),
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

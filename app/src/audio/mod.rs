//! Audio engine: device I/O, the DSP tick, and per-peer receive state.
//!
//! Signal flow, once per 2.5 ms block on the DSP thread (`mixer`):
//!
//! ```text
//!  mic ─► AEC(ref = last mix) ─► analysis/VAD/profile ─┬─► gate ─► QUIC datagrams to peers
//!                                                      └─► crosstalk reference
//!  peer voice ─► jitter ─► crosstalk cancel ─► residual suppress ─► gain ─┐
//!  peer screen audio (stereo) ─► jitter ─► stream volume ───────────────┐ │
//!                                                              mix ◄────┴─┘
//!  mix ─► limiter ─► drift-corrected resampler ─► playback ─► speakers
//! ```

pub mod device;
pub mod dsp;
pub mod jitter;
#[cfg(windows)]
pub mod loopback;
mod recorder;
#[cfg(windows)]
pub mod wasapi;
mod mixer;
mod resample;


use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

use parking_lot::{Mutex, RwLock};

use crate::clock;
use crate::protocol::{self, BLOCK, PeerId};
pub use device::{DeviceSpec, DeviceStatus, Driver};
use jitter::{JitterBuffer, JitterStats};

/// Interleaved stereo block of the same duration as a voice block.
pub const STEREO_BLOCK: usize = BLOCK * 2;

/// Lock-free f32 cell for meters and settings read on the DSP thread.
#[derive(Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn load(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn store(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed)
    }
}

/// Receive-side state for one peer, shared between the network and DSP threads.
pub struct PeerAudio {
    pub id: PeerId,
    /// Their QUIC connection: our voice goes out on it as datagrams.
    pub link: crate::net::Link,
    voice: Mutex<JitterBuffer<BLOCK>>,
    stream: Mutex<JitterBuffer<STEREO_BLOCK>>,
    pub volume: AtomicF32,
    pub level: AtomicF32,
    pub voice_active: AtomicBool,
    pub colocated: AtomicBool,
    pub coupling: AtomicF32,
    pub last_packet_us: AtomicU64,
    pub last_stream_us: AtomicU64,
    pub stats: Mutex<JitterStats>,
    /// In the same channel as us: we exchange voice. Others stay connected
    /// (presence, pings, screen shares) but are neither sent to nor mixed.
    in_channel: AtomicBool,
    /// What the network delivered, before any buffering.
    pub net: Mutex<NetStats>,
    /// Screen-share audio: buffer stats and what the network delivered.
    pub stream_stats: Mutex<JitterStats>,
    pub stream_net: Mutex<NetStats>,
}

/// Arrival statistics for one peer's voice packets, independent of the
/// jitter buffer: real loss vs. late or bursty delivery.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetStats {
    pub received: u64,
    /// Sequence numbers spanned (highest − first + 1): received + lost.
    pub expected: u64,
    pub reordered: u64,
    /// Arrival gaps longer than 10 / 20 / 50 ms (the sender sends every 2.5 ms,
    /// in bursts of up to 10 ms in shared mode).
    pub gaps_10ms: u64,
    pub gaps_20ms: u64,
    pub gaps_50ms: u64,
    pub max_gap_us: u64,
    first: Option<u32>,
    highest: u32,
    last_arrival_us: u64,
}

impl NetStats {
    fn record(&mut self, seq: u32, now: u64) {
        let first = *self.first.get_or_insert(seq);
        if self.received > 0 {
            if seq < self.highest {
                self.reordered += 1;
            }
            let gap = now.saturating_sub(self.last_arrival_us);
            self.gaps_10ms += (gap > 10_000) as u64;
            self.gaps_20ms += (gap > 20_000) as u64;
            self.gaps_50ms += (gap > 50_000) as u64;
            self.max_gap_us = self.max_gap_us.max(gap);
        }
        self.highest = self.highest.max(seq);
        self.received += 1;
        self.expected = (self.highest.wrapping_sub(first) as u64 + 1).max(self.received);
        self.last_arrival_us = now;
    }
}

impl PeerAudio {
    pub fn streaming(&self) -> bool {
        clock::now_us().saturating_sub(self.last_stream_us.load(Ordering::Relaxed)) < 300_000
    }

    pub fn in_channel(&self) -> bool {
        self.in_channel.load(Ordering::Relaxed)
    }

    pub fn set_in_channel(&self, on: bool) {
        if self.in_channel.swap(on, Ordering::Relaxed) == on {
            return;
        }
        if on {
            // Their sequence numbers moved on while we weren't listening.
            *self.voice.lock() = JitterBuffer::new(1);
            *self.net.lock() = NetStats::default();
        } else {
            self.voice_active.store(false, Ordering::Relaxed);
            self.level.store(0.0);
        }
    }
}

pub struct AudioShared {
    me: PeerId,

    peers: RwLock<Vec<Arc<PeerAudio>>>,
    pub muted: AtomicBool,
    pub deafened: AtomicBool,
    pub echo_cancel: AtomicBool,
    pub crosstalk_cancel: AtomicBool,
    pub noise_gate: AtomicBool,
    pub stream_volume: AtomicF32,
    /// Manual microphone gain (linear), set by the user for quiet mics.
    pub mic_gain: AtomicF32,
    /// The voice profile as the UI shows it, refreshed by the DSP ~50 times/s.
    pub profile_view: Mutex<ProfileView>,
    /// Edits from the UI, applied on the next DSP tick.
    profile_edits: Mutex<Vec<ProfileEdit>>,
    /// Keep training the profile from our speech (off: frozen as it is).
    pub profile_learning: AtomicBool,
    /// How closely speech must match the profile to open the gate (-1..1).
    pub gate_threshold: AtomicF32,
    pub local_level: AtomicF32,
    pub local_voice: AtomicBool,
    pub profile_progress: AtomicF32,
    pub echo_delay_ms: AtomicF32,
    /// Worst DSP tick of the last second as a fraction of the 2.5 ms budget.
    pub dsp_load: AtomicF32,
    reset_profile: AtomicBool,
    pub status: Arc<Mutex<DeviceStatus>>,
    counters: Arc<device::Counters>,
    spec: Mutex<DeviceSpec>,
    dev_tx: Mutex<Option<crossbeam_channel::Sender<device::DeviceCmd>>>,
    profile_path: PathBuf,
    /// Set by the UI: save the diagnostic recording now.
    snapshot_request: AtomicBool,
    snapshot_result: Mutex<Option<Result<PathBuf, String>>>,
}

pub struct AudioSettings {
    pub spec: DeviceSpec,
    pub echo_cancel: bool,
    pub crosstalk_cancel: bool,
    pub noise_gate: bool,
    pub stream_volume: f32,
    pub mic_gain_db: f32,
    pub profile_learning: bool,
    pub gate_threshold: f32,
    pub profile_path: PathBuf,
}

impl AudioShared {
    pub fn start(me: PeerId, settings: AudioSettings) -> Arc<Self> {
        let (io_tx, io_rx) = crossbeam_channel::bounded(2);
        let shared = Arc::new(Self {
            me,

            peers: RwLock::new(Vec::new()),
            muted: AtomicBool::new(false),
            deafened: AtomicBool::new(false),
            echo_cancel: AtomicBool::new(settings.echo_cancel),
            crosstalk_cancel: AtomicBool::new(settings.crosstalk_cancel),
            noise_gate: AtomicBool::new(settings.noise_gate),
            stream_volume: AtomicF32::new(settings.stream_volume),
            mic_gain: AtomicF32::new(db_to_gain(settings.mic_gain_db)),
            profile_view: Mutex::new(ProfileView::default()),
            profile_edits: Mutex::new(Vec::new()),
            profile_learning: AtomicBool::new(settings.profile_learning),
            gate_threshold: AtomicF32::new(settings.gate_threshold),
            local_level: AtomicF32::new(0.0),
            local_voice: AtomicBool::new(false),
            profile_progress: AtomicF32::new(0.0),
            echo_delay_ms: AtomicF32::new(-1.0),
            dsp_load: AtomicF32::new(0.0),
            reset_profile: AtomicBool::new(false),
            status: Arc::new(Mutex::new(DeviceStatus::default())),
            counters: Arc::new(device::Counters::default()),
            spec: Mutex::new(settings.spec.clone()),
            dev_tx: Mutex::new(None),
            profile_path: settings.profile_path,
            snapshot_request: AtomicBool::new(false),
            snapshot_result: Mutex::new(None),
        });

        let s = shared.clone();
        let dsp = thread::Builder::new()
            .name("audio-dsp".into())
            .spawn(move || {
                let _ = thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max);
                mixer::Mixer::new(s, io_rx).run()
            })
            .expect("spawn dsp thread");

        let tx = device::spawn(
            settings.spec,
            shared.counters.clone(),
            dsp.thread().clone(),
            shared.status.clone(),
            io_tx,
        );
        *shared.dev_tx.lock() = Some(tx);
        shared
    }

    pub fn add_peer(&self, id: PeerId, link: crate::net::Link, volume: f32) -> Arc<PeerAudio> {
        let peer = Arc::new(PeerAudio {
            id,
            link,
            voice: Mutex::new(JitterBuffer::new(1)),
            // System audio arrives in 10 ms bursts (the sender's capture period).
            stream: Mutex::new(JitterBuffer::new(2).with_min_target(4)),
            volume: AtomicF32::new(volume),
            level: AtomicF32::new(0.0),
            voice_active: AtomicBool::new(false),
            colocated: AtomicBool::new(false),
            coupling: AtomicF32::new(0.0),
            last_packet_us: AtomicU64::new(0),
            last_stream_us: AtomicU64::new(0),
            stats: Mutex::new(JitterStats::default()),
            in_channel: AtomicBool::new(true),
            net: Mutex::new(NetStats::default()),
            stream_stats: Mutex::new(JitterStats::default()),
            stream_net: Mutex::new(NetStats::default()),
        });
        let mut peers = self.peers.write();
        peers.retain(|p| p.id != id);
        peers.push(peer.clone());
        peer
    }

    pub fn remove_peer(&self, id: PeerId) {
        self.peers.write().retain(|p| p.id != id);
    }

    pub fn peer(&self, id: PeerId) -> Option<Arc<PeerAudio>> {
        self.peers.read().iter().find(|p| p.id == id).cloned()
    }

    /// Voice packet from the network.
    pub fn receive(&self, sender: PeerId, seq: u32, flags: u8, pcm: &[u8]) {
        let Some(peer) = self.peer(sender) else { return };
        if !peer.in_channel() {
            return; // stragglers from before a channel switch
        }
        let mut block = [0f32; BLOCK];
        protocol::decode_pcm(pcm, &mut block);
        let now = clock::now_us();
        peer.last_packet_us.store(now, Ordering::Relaxed);
        peer.voice_active.store(flags & protocol::AUDIO_FLAG_VOICE != 0, Ordering::Relaxed);
        peer.net.lock().record(seq, now);
        peer.voice.lock().push(seq, block, now);
    }

    /// Screen-share audio packet (interleaved stereo).
    pub fn receive_stream(&self, sender: PeerId, seq: u32, pcm: &[u8]) {
        let Some(peer) = self.peer(sender) else { return };
        let mut block = [0f32; STEREO_BLOCK];
        protocol::decode_pcm(pcm, &mut block);
        let now = clock::now_us();
        peer.last_stream_us.store(now, Ordering::Relaxed);
        peer.stream_net.lock().record(seq, now);
        peer.stream.lock().push(seq, block, now);
    }

    pub fn set_spec(&self, spec: DeviceSpec) {
        *self.spec.lock() = spec.clone();
        if let Some(tx) = self.dev_tx.lock().as_ref() {
            let _ = tx.send(device::DeviceCmd::Rebuild(spec));
        }
    }

    /// Saves the last [`recorder::SECONDS`] of every audio stage; poll
    /// [`Self::take_snapshot_result`] for the folder.
    pub fn request_snapshot(&self) {
        self.snapshot_request.store(true, Ordering::Relaxed);
    }

    pub fn take_snapshot_result(&self) -> Option<Result<PathBuf, String>> {
        self.snapshot_result.lock().take()
    }

    pub fn edit_profile(&self, edit: ProfileEdit) {
        self.profile_edits.lock().push(edit);
    }

    /// Pending UI edits (the DSP thread takes them without waiting).
    fn take_profile_edits(&self) -> Vec<ProfileEdit> {
        self.profile_edits.try_lock().map(|mut e| std::mem::take(&mut *e)).unwrap_or_default()
    }

    pub fn reset_voice_profile(&self) {
        self.reset_profile.store(true, Ordering::Relaxed);
    }

    pub fn send_errors(&self) -> u64 {
        self.counters.send_errors.load(Ordering::Relaxed)
    }

    pub fn playback_underruns(&self) -> u64 {
        self.counters.output_underruns.load(Ordering::Relaxed)
    }
}

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// What the profile editor draws.
#[derive(Debug, Clone, Default)]
pub struct ProfileView {
    /// Learned shape and its spread (dB relative to the voice's average).
    pub mean: dsp::profile::Bands,
    pub spread: dsp::profile::Bands,
    pub ignored: [bool; dsp::profile::BANDS],
    /// The mic right now, normalised the same way (what gets compared).
    pub live: dsp::profile::Bands,
    /// Live match against the profile, -1..1.
    pub similarity: f32,
    pub speaking: bool,
    /// Trained enough for the gate to use it.
    pub usable: bool,
    pub seconds: f32,
}

#[derive(Debug, Clone, Copy)]
pub enum ProfileEdit {
    SetBand(usize, f32),
    ToggleIgnored(usize),
}

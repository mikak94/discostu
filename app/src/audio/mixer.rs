//! The DSP thread: one tick per 2.5 ms block of captured audio.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use rtrb::{Consumer, Producer};

use super::device::DeviceIo;
use super::dsp::echo::{EchoCanceller, ramp};
use super::dsp::profile::{self as voice, Analyzer, Bands, NoiseFloor, Vad, VoiceProfile};
use super::dsp::talker::{Talker, TalkerDetector};
use super::recorder::{self, PeerTick, Recorder};
use super::dsp::{History, coeff, db, energy};
use super::resample::Resampler;
use super::jitter::{JitterBuffer, JitterStats};
use super::{AudioShared, ProfileEdit, STEREO_BLOCK};
use crate::clock;
use crate::protocol::{self, BLOCK, PeerId, SAMPLE_RATE};

type Block = [f32; BLOCK];

/// Gate stays open this long after the last voiced frame (blocks).
const GATE_HOLD: u32 = 100; // 250 ms
/// A roommate's turn lasts at least this long once detected (blocks).
const ROOMMATE_HOLD: u32 = 40; // 100 ms
const ROOM_LOOKBACK: usize = 4096;

fn soft_limit(block: &mut [f32]) {
    for s in block {
        *s = super::dsp::soft_limit(*s);
    }
}

struct Capture {
    cons: Consumer<f32>,
    resampler: Resampler,
}

struct Playback {
    prod: Producer<f32>,
    resampler: Resampler,
    /// Device frames produced per tick (≈ 120 at 48 kHz).
    frames_per_tick: f64,
    capacity: usize,
    window_ticks: u32,
    /// Device underrun count at the last tick.
    underruns: u64,
    correction: f64,
    /// Slack (frames) the controller keeps for the device. Grows when the
    /// device runs dry, shrinks back after a quiet spell.
    headroom: f64,
    /// Ticks since the last underrun.
    calm_ticks: u32,
}

struct PeerDsp {
    crosstalk: EchoCanceller,
    /// Correlates their stream against our (delayed) mic to spot shared rooms
    /// even when their gate keeps our voice out of their stream.
    room: super::dsp::delay::DelayEstimator,
    room_line: History,
    room_tick: u32,
    room_score: f32,
    colocated: bool,
    /// Same room: is it us or them talking right now?
    talker: TalkerDetector,
    gain: f32,
    stream_gain: f32,
    /// Their screen-share audio, time-stretched to hold its buffer steady.
    stream_rs: Resampler,
    stream_fifo: Vec<f32>,
    stream_corr: f64,
}

impl PeerDsp {
    fn new() -> Self {
        Self {
            crosstalk: EchoCanceller::new(768, 4096),
            room: super::dsp::delay::DelayEstimator::new(4096, ROOM_LOOKBACK * 2),
            room_line: History::new(ROOM_LOOKBACK + BLOCK),
            room_tick: 0,
            room_score: 0.0,
            colocated: false,
            talker: TalkerDetector::new(),
            gain: 0.0,
            stream_gain: 0.0,
            stream_rs: Resampler::with_channels(SAMPLE_RATE, SAMPLE_RATE, 2),
            stream_fifo: Vec::with_capacity(STEREO_BLOCK * 2),
            stream_corr: 0.0,
        }
    }

    /// Next stereo block of their screen-share audio. Their sound card and
    /// our clock never quite agree; instead of dropping or repeating blocks
    /// (which crackles on music) the stream is stretched by up to ±0.2 %,
    /// inaudible, to keep its buffer at the target depth.
    fn next_stream_block(
        &mut self,
        jb: &parking_lot::Mutex<JitterBuffer<STEREO_BLOCK>>,
        stats: &parking_lot::Mutex<JitterStats>,
    ) -> [f32; STEREO_BLOCK] {
        while self.stream_fifo.len() < STEREO_BLOCK {
            let (blk, depth, target) = {
                let mut jb = jb.lock();
                let b = jb.pop();
                let st = jb.stats();
                *stats.lock() = st;
                (b, jb.depth(), st.target)
            };
            let error = depth as f64 - target as f64;
            let want = (error / 4.0).clamp(-1.0, 1.0) * 0.002;
            self.stream_corr += 0.02 * (want - self.stream_corr);
            self.stream_rs.set_correction(self.stream_corr);
            self.stream_rs.process_interleaved(&blk, &mut self.stream_fifo);
        }
        let mut out = [0f32; STEREO_BLOCK];
        out.copy_from_slice(&self.stream_fifo[..STEREO_BLOCK]);
        self.stream_fifo.drain(..STEREO_BLOCK);
        out
    }
}

pub struct Mixer {
    shared: Arc<AudioShared>,
    io_rx: Receiver<DeviceIo>,
    capture: Option<Capture>,
    playback: Option<Playback>,
    cap_fifo: Vec<f32>,
    scratch: Vec<f32>,
    aec: EchoCanceller,
    analyzer: Analyzer,
    noise: NoiseFloor,
    vad: Vad,
    profile: VoiceProfile,
    profile_dirty: bool,
    last_save: Instant,
    similarity: f32,
    /// The mic's band shape, smoothed for the profile editor.
    live_shape: Bands,
    view_ticks: u32,
    speech_level: f32,
    gate: f32,
    gate_hold: u32,
    /// Manual mic gain as applied (ramps toward the setting).
    mic_gain: f32,
    /// Last tick: someone in our room is talking and we aren't. Their voice
    /// in our mic must not go out, or train our voice profile.
    roommate_talking: bool,
    /// Keeps `roommate_talking` set through their short pauses (blocks), so
    /// our gate doesn't flap open and shut between their syllables.
    roommate_hold: u32,
    /// Last tick: the speech in our mic is ours alone, safe to learn from.
    learn_ok: bool,
    /// Mono sum of what went to the speakers last tick (AEC reference).
    last_mix: Block,
    peers: HashMap<PeerId, PeerDsp>,
    seq: u32,
    packet: Vec<u8>,
    clock_start: Instant,
    clock_ticks: u64,
    load_max_us: u64,
    load_ticks: u32,
    /// Last 10 s of every stage, for the "save diagnostics" button.
    recorder: Recorder,
    rec_peers: Vec<(PeerId, Block)>,
    rec_peer_ticks: [PeerTick; recorder::MAX_PEERS],
}

impl Mixer {
    pub fn new(shared: Arc<AudioShared>, io_rx: Receiver<DeviceIo>) -> Self {
        let profile = std::fs::read(&shared.profile_path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self {
            shared,
            io_rx,
            capture: None,
            playback: None,
            cap_fifo: Vec::with_capacity(SAMPLE_RATE as usize),
            scratch: Vec::with_capacity(4096),
            // 1024 taps = 21 ms of room tail after a bulk delay of up to 256 ms.
            aec: EchoCanceller::new(1024, 12288),
            analyzer: Analyzer::new(),
            noise: NoiseFloor::default(),
            vad: Vad::default(),
            profile,
            profile_dirty: false,
            last_save: Instant::now(),
            similarity: 0.0,
            live_shape: [0.0; voice::BANDS],
            view_ticks: 0,
            speech_level: -40.0,
            gate: 0.0,
            gate_hold: 0,
            mic_gain: 1.0,
            roommate_talking: false,
            roommate_hold: 0,
            learn_ok: true,
            last_mix: [0.0; BLOCK],
            peers: HashMap::new(),
            seq: 0,
            packet: Vec::with_capacity(protocol::MAX_DATAGRAM),
            clock_start: Instant::now(),
            clock_ticks: 0,
            load_max_us: 0,
            load_ticks: 0,
            recorder: Recorder::new(),
            rec_peers: Vec::with_capacity(recorder::MAX_PEERS),
            rec_peer_ticks: [PeerTick::default(); recorder::MAX_PEERS],
        }
    }

    pub fn run(mut self) {
        loop {
            while let Ok(io) = self.io_rx.try_recv() {
                self.capture = io.capture.map(|(cons, rate)| Capture {
                    cons,
                    resampler: Resampler::new(rate, SAMPLE_RATE),
                });
                self.playback = io.playback.map(|(prod, rate)| Playback {
                    capacity: prod.buffer().capacity(),
                    prod,
                    resampler: Resampler::with_channels(SAMPLE_RATE, rate, 2),
                    frames_per_tick: BLOCK as f64 * rate as f64 / SAMPLE_RATE as f64,
                    window_ticks: 0,
                    underruns: self.shared.counters.output_underruns.load(Ordering::Relaxed),
                    correction: 0.0,
                    headroom: (BLOCK as f64 * rate as f64 / SAMPLE_RATE as f64) * 0.5,
                    calm_ticks: 0,
                });
                self.cap_fifo.clear();
                self.clock_start = Instant::now();
                self.clock_ticks = 0;
            }

            match &mut self.capture {
                Some(cap) => {
                    if cap.cons.slots() == 0 {
                        thread::park_timeout(Duration::from_millis(5));
                    }
                    let n = cap.cons.slots();
                    if let Ok(chunk) = cap.cons.read_chunk(n) {
                        let (a, b) = chunk.as_slices();
                        cap.resampler.process(a, &mut self.cap_fifo);
                        cap.resampler.process(b, &mut self.cap_fifo);
                        chunk.commit_all();
                    }
                }
                None => {
                    // No microphone: run the mixer off the wall clock instead.
                    thread::park_timeout(Duration::from_micros(2500));
                    let due = (self.clock_start.elapsed().as_micros() as u64 * SAMPLE_RATE as u64
                        / 1_000_000)
                        / BLOCK as u64;
                    while self.clock_ticks < due {
                        self.cap_fifo.extend_from_slice(&[0.0; BLOCK]);
                        self.clock_ticks += 1;
                    }
                }
            }

            let mut offset = 0;
            while self.cap_fifo.len() - offset >= BLOCK {
                let mut block = [0f32; BLOCK];
                block.copy_from_slice(&self.cap_fifo[offset..offset + BLOCK]);
                offset += BLOCK;
                let started = Instant::now();
                self.tick(&mut block);
                self.account(started.elapsed().as_micros() as u64);
            }
            self.cap_fifo.drain(..offset);

            if self.profile_dirty && self.last_save.elapsed() > Duration::from_secs(30) {
                self.save_profile();
            }
        }
    }

    fn account(&mut self, us: u64) {
        self.load_max_us = self.load_max_us.max(us);
        self.load_ticks += 1;
        if self.load_ticks >= 400 {
            self.shared.dsp_load.store(self.load_max_us as f32 / 2500.0);
            self.load_max_us = 0;
            self.load_ticks = 0;
        }
    }

    /// Hands the profile editor a fresh picture every 20 ms.
    fn publish_profile(&mut self, bands: &Bands, speaking: bool) {
        let s = voice::shape(bands, &self.profile.ignored);
        for (l, v) in self.live_shape.iter_mut().zip(s) {
            *l += 0.35 * (v - *l);
        }
        self.view_ticks += 1;
        if self.view_ticks < 8 {
            return;
        }
        self.view_ticks = 0;
        if let Some(mut v) = self.shared.profile_view.try_lock() {
            v.mean = self.profile.mean;
            v.spread = self.profile.var.map(f32::sqrt);
            v.ignored = self.profile.ignored;
            v.live = self.live_shape;
            v.similarity = self.similarity;
            v.speaking = speaking;
            v.usable = self.profile.usable();
            v.seconds = self.profile.seconds();
        }
    }

    fn save_profile(&mut self) {
        if let Ok(json) = serde_json::to_vec(&self.profile) {
            if let Some(dir) = self.shared.profile_path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&self.shared.profile_path, json);
        }
        self.profile_dirty = false;
        self.last_save = Instant::now();
    }

    fn tick(&mut self, mic: &mut Block) {
        let sh = self.shared.clone();
        let flag = |a: &AtomicBool| a.load(Ordering::Relaxed);
        let mic_in = *mic;

        // 0. Manual mic gain (Settings), ramped so moving the slider never
        //    clicks. The recording above keeps the raw device level.
        let gain_target = sh.mic_gain.load();
        if gain_target != 1.0 || self.mic_gain != 1.0 {
            let start = self.mic_gain;
            self.mic_gain += 0.3 * (gain_target - self.mic_gain);
            if (self.mic_gain - gain_target).abs() < 1e-4 {
                self.mic_gain = gain_target;
            }
            ramp(mic, start, self.mic_gain);
        }
        self.rec_peers.clear();
        self.rec_peer_ticks = [PeerTick::default(); recorder::MAX_PEERS];

        if sh.reset_profile.swap(false, Ordering::Relaxed) {
            self.profile = VoiceProfile::default();
            self.save_profile();
        }
        let edits = sh.take_profile_edits();
        if !edits.is_empty() {
            for e in edits {
                match e {
                    ProfileEdit::SetBand(b, db) => self.profile.set_band(b, db),
                    ProfileEdit::ToggleIgnored(b) => self.profile.toggle_ignored(b),
                }
            }
            self.profile_dirty = true;
        }

        // 1. Acoustic echo cancellation against what we played last tick.
        if flag(&sh.echo_cancel) {
            self.aec.process(&self.last_mix, mic, true, true);
            let d = self.aec.stats().delay;
            sh.echo_delay_ms
                .store(d.map_or(-1.0, |d| d as f32 * 1000.0 / SAMPLE_RATE as f32));
        }

        // 2. Analysis: noise floor, VAD, voice profile.
        self.analyzer.push(mic);
        let bands = self.analyzer.analyze();
        let level_db = db(energy(mic) / BLOCK as f32);
        let snr = self.noise.update(&bands);
        let speaking = self.vad.update(snr, level_db, 60);
        let sim = self.profile.similarity(&bands);
        self.similarity += coeff(80.0) * (sim - self.similarity);
        let echo_dominant = flag(&sh.echo_cancel) && self.aec.stats().echo_ratio > 0.3;
        // Track how loud confident speech gets (fast up, slow down). Our own
        // voice, close to the mic, is the loudest; a video playing across the
        // room is not — so only near-peak speech trains the profile.
        if speaking && snr > 10.0 && self.learn_ok {
            let k = if level_db > self.speech_level { 0.05 } else { 0.0005 };
            self.speech_level += k * (level_db - self.speech_level);
        }
        let live = !flag(&sh.muted) && !flag(&sh.deafened);
        // Only learn speech that is ours alone: a roommate talking more than us
        // would otherwise slowly turn our profile into theirs.
        if live
            && flag(&sh.profile_learning)
            && speaking
            && snr > 14.0
            && level_db > self.speech_level - 10.0
            && !echo_dominant
            && self.learn_ok
        {
            self.profile.learn(&bands);
            self.profile_dirty = true;
        }
        sh.profile_progress.store(self.profile.progress());
        self.publish_profile(&bands, speaking);
        let me_talking = speaking && (!self.profile.usable() || self.similarity > 0.3);

        // Soft limiter instead of hard clipping at the 16-bit conversion.
        soft_limit(mic);

        // Our own voice, pre-gate: reference for same-room detection.
        let reference = *mic;

        // 3. Gate: open on speech that sounds like us, then hold through
        //    pauses so words are never chopped.
        //    A roommate talking alone closes it outright (same-room
        //    separation): their voice reached our mic through the air, and they
        //    must not hear it come back.
        let roommate = flag(&sh.crosstalk_cancel) && self.roommate_talking;
        let voiced =
            speaking && !roommate && (!self.profile.usable() || self.similarity > sh.gate_threshold.load());
        if voiced {
            self.gate_hold = GATE_HOLD;
        } else if roommate {
            self.gate_hold = 0;
        } else {
            self.gate_hold = self.gate_hold.saturating_sub(1);
        }
        let open = !roommate && (!flag(&sh.noise_gate) || voiced || self.gate_hold > 0);
        let target = if open { 1.0 } else { 0.03 };
        let k = if target > self.gate { 0.6 } else if roommate { 0.3 } else { coeff(30.0) };
        let start = self.gate;
        self.gate += k * (target - self.gate);
        ramp(mic, start, self.gate);

        let rms = (energy(mic) / BLOCK as f32).sqrt();
        sh.local_level.store(rms);
        sh.local_voice.store(open && speaking, Ordering::Relaxed);

        // 4. Send to everyone. Muted still sends silence so peers' jitter
        //    buffers stay primed and unmuting is instant.
        let silent = !live;
        let out: &[f32] = if silent { &[0.0; BLOCK] } else { mic };
        let flags = if !silent && open && speaking { protocol::AUDIO_FLAG_VOICE } else { 0 };
        protocol::write_audio(&mut self.packet, sh.me, self.seq, clock::now_us(), flags, out);
        let sent: Block = if silent { [0.0; BLOCK] } else { *mic };
        self.seq = self.seq.wrapping_add(1);

        // 5. Receive and mix (stereo).
        let mut mix = [0f32; STEREO_BLOCK];
        let stream_volume = sh.stream_volume.load();
        let mic_power = energy(&reference) / BLOCK as f32;
        let (mut roommate_talking, mut we_talk, mut learn_ok) = (false, false, true);
        let (mut rec_stream, mut rec_stream_set) = ([0f32; BLOCK], false);
        {
            let peers = sh.peers.read();
            self.peers.retain(|id, _| peers.iter().any(|p| p.id == *id));
            for peer in peers.iter() {
                // Voice only flows within a channel. Everyone else is silence
                // here (their screen-share audio below still plays if we watch).
                let in_channel = peer.in_channel();
                if in_channel && !peer.link.send(&self.packet) {
                    sh.counters.send_errors.fetch_add(1, Ordering::Relaxed);
                }
                let pd = self.peers.entry(peer.id).or_insert_with(PeerDsp::new);
                let mut blk = [0f32; BLOCK];
                if in_channel {
                    let mut jb = peer.voice.lock();
                    blk = jb.pop();
                    *peer.stats.lock() = jb.stats();
                }
                let their_voice = peer.voice_active.load(Ordering::Relaxed);
                let talker = pd.talker.update(mic_power, energy(&blk) / BLOCK as f32, in_channel && (speaking || their_voice));
                if self.rec_peers.len() < recorder::MAX_PEERS {
                    let st = *peer.stats.lock();
                    self.rec_peer_ticks[self.rec_peers.len()] = PeerTick {
                        id: peer.id,
                        talker: talker as u8,
                        voice: their_voice,
                        target: st.target as u16,
                        lost: st.lost,
                        underruns: st.underruns,
                        late: st.late,
                    };
                    self.rec_peers.push((peer.id, blk));
                }
                // Applies to everyone in the channel, not just peers detected as
                // being in our room: that detection needs their exact waveform in
                // our mic and often misses across two mics and a reverberant room.
                // For a remote peer it's harmless: they talk alone, we're silent.
                if in_channel {
                    match talker {
                        Talker::Them => (roommate_talking, learn_ok) = (true, false),
                        Talker::Both => (we_talk, learn_ok) = (true, false),
                        Talker::Us => we_talk = true,
                        Talker::Unknown if their_voice => learn_ok = false,
                        Talker::Unknown => {}
                    }
                }

                // Room detection: does their voice show up in our mic? Our mic
                // may hear them before their packets arrive, so delay the mic
                // by ROOM_LOOKBACK to keep both signs of lag in range.
                for &s in reference.iter() {
                    pd.room_line.push(s);
                }
                let delayed_mic: Block =
                    std::array::from_fn(|i| pd.room_line.get(ROOM_LOOKBACK + BLOCK - 1 - i));
                let they_alone = talker == Talker::Them || (talker == Talker::Unknown && !me_talking);
                if their_voice && they_alone {
                    pd.room.push(&blk, &delayed_mic);
                } else {
                    pd.room.push(&[0.0; BLOCK], &[0.0; BLOCK]);
                }
                pd.room_tick += 1;
                if pd.room_tick % 40 == 0 {
                    pd.room.update();
                    let found = pd.room.delay().is_some() && pd.room.confidence() > 0.55;
                    pd.room_score += 0.1 * ((found as u8 as f32) - pd.room_score);
                }

                // Coupling probe: how strongly our voice shows up in their mic,
                // for same-room detection. It runs on a copy and never touches
                // their stream. Subtracting with our mic as the reference also
                // ate their voice whenever it reached our mic, which crackled.
                // Their side keeps our voice out instead: same-room separation
                // closes their mic while we talk alone.
                {
                    let ours = match talker {
                        Talker::Us => speaking,
                        Talker::Unknown => me_talking && !their_voice,
                        Talker::Them | Talker::Both => false,
                    };
                    let mut probe = blk;
                    pd.crosstalk.process(&reference, &mut probe, ours, false);
                }
                let coupling = pd.crosstalk.stats().coupling;
                peer.coupling.store(coupling.max(pd.room_score));

                // Same-room detection with hysteresis (shown in the UI only).
                let score = coupling.max(pd.room_score);
                pd.colocated = if pd.colocated { score > 0.12 } else { score > 0.35 };
                peer.colocated.store(pd.colocated, Ordering::Relaxed);

                let target = if in_channel { peer.volume.load() } else { 0.0 };
                let start = pd.gain;
                pd.gain += coeff(8.0) * (target - pd.gain);
                peer.level.store((energy(&blk) / BLOCK as f32).sqrt());
                let n = BLOCK as f32;
                for (i, s) in blk.iter().enumerate() {
                    let g = start + (pd.gain - start) * (i as f32 + 1.0) / n;
                    mix[2 * i] += s * g;
                    mix[2 * i + 1] += s * g;
                }

                // Their screen-share audio, if we're watching them.
                let streaming = peer.streaming();
                let stream_target = if streaming { stream_volume } else { 0.0 };
                if streaming || pd.stream_gain > 1e-4 {
                    let sblk = pd.next_stream_block(&peer.stream, &peer.stream_stats);
                    if !rec_stream_set {
                        for (f, frame) in sblk.chunks_exact(2).enumerate() {
                            rec_stream[f] = 0.5 * (frame[0] + frame[1]);
                        }
                        rec_stream_set = true;
                    }
                    let start = pd.stream_gain;
                    pd.stream_gain += coeff(8.0) * (stream_target - pd.stream_gain);
                    for (f, frame) in sblk.chunks_exact(2).enumerate() {
                        let g = start + (pd.stream_gain - start) * (f as f32 + 1.0) / n;
                        mix[2 * f] += frame[0] * g;
                        mix[2 * f + 1] += frame[1] * g;
                    }
                }
            }
        }

        // The hold bridges their syllables, but our own voice ends it at once:
        // talking over them must never lose our first word.
        self.roommate_hold = if we_talk {
            0
        } else if roommate_talking {
            ROOMMATE_HOLD
        } else {
            self.roommate_hold.saturating_sub(1)
        };
        self.roommate_talking = self.roommate_hold > 0;
        self.learn_ok = learn_ok;

        // 6. Soft limiter, then playback.
        soft_limit(&mut mix);
        if flag(&sh.deafened) {
            mix = [0.0; STEREO_BLOCK];
        }
        self.push_playback(&mix);
        for (i, m) in self.last_mix.iter_mut().enumerate() {
            *m = 0.5 * (mix[2 * i] + mix[2 * i + 1]);
        }

        let (fill, headroom) = self.playback.as_ref().map_or((0, 0.0), |pb| {
            (((pb.capacity - pb.prod.slots()) / 2) as u32, pb.headroom as f32)
        });
        let counters = &sh.counters;
        self.recorder.record(
            &mic_in,
            &sent,
            &self.last_mix,
            &rec_stream,
            &self.rec_peers,
            recorder::Tick {
                t_us: clock::now_us(),
                speaking,
                roommate: self.roommate_talking,
                gate: self.gate,
                fill,
                headroom,
                out_underruns: counters.output_underruns.load(Ordering::Relaxed),
                xruns: counters.xruns.load(Ordering::Relaxed),
                peers: self.rec_peer_ticks,
            },
        );
        if sh.snapshot_request.swap(false, Ordering::Relaxed) {
            self.save_snapshot();
        }
    }

    /// Copies the recording (a few ms, once) and writes it on another thread.
    fn save_snapshot(&self) {
        let snap = self.recorder.snapshot();
        let sh = self.shared.clone();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let dir = sh.profile_path.parent().unwrap_or(std::path::Path::new(".")).join("diagnostics").join(stamp.to_string());
        let info = format!("{:#?}
", *sh.status.lock());
        thread::spawn(move || {
            let result = snap.save(&dir, &info).map(|()| dir).map_err(|e| e.to_string());
            *sh.snapshot_result.lock() = Some(result);
        });
    }

    fn push_playback(&mut self, mix: &[f32; STEREO_BLOCK]) {
        let Some(pb) = &mut self.playback else { return };
        let counters = &self.shared.counters;

        // Clock drift between capture and playback (WASAPI: two devices) is
        // absorbed by nudging the resampling ratio a fraction of a percent,
        // steering the slack the *device* sees when it reads (ring fill minus
        // what it takes) toward `headroom`. Measuring at the reader matters:
        // it reads in its own period sizes (e.g. 144 or 480 frames), not ours.
        //
        // Capture and playback deliver in bursts (10 ms in shared mode) on
        // separate clocks; when their timing drifts past each other the slack
        // the device needs jumps by up to a whole burst. So headroom adapts:
        // it starts at half a block, grows a block per underrun, and gives a
        // quarter block back after every 10 s without one.
        let fill = (pb.capacity - pb.prod.slots()) / 2;
        pb.window_ticks += 1;
        if pb.window_ticks >= 40 {
            let slack = counters.output_slack.swap(usize::MAX, Ordering::Relaxed);
            if slack != usize::MAX {
                let target = pb.headroom.max(32.0);
                let error = slack as f64 - target;
                let wanted = (error / pb.frames_per_tick).clamp(-1.0, 1.0) * 0.003;
                pb.correction += 0.3 * (wanted - pb.correction);
                pb.resampler.set_correction(pb.correction);
            }
            pb.window_ticks = 0;
        }
        // The device ran dry: add half a block of slack right away (into the
        // gap it just played) instead of waiting for the slow correction.
        let underruns = counters.output_underruns.load(Ordering::Relaxed);
        let dry = underruns != pb.underruns;
        let prefill = dry && (fill as f64) < pb.headroom + pb.frames_per_tick;
        pb.underruns = underruns;
        if dry {
            pb.headroom = (pb.headroom + pb.frames_per_tick).min(pb.frames_per_tick * 16.0);
            pb.calm_ticks = 0;
        } else {
            pb.calm_ticks += 1;
            if pb.calm_ticks >= 4000 {
                pb.calm_ticks = 0;
                pb.headroom = (pb.headroom - pb.frames_per_tick * 0.25).max(pb.frames_per_tick * 0.5);
            }
        }
        if fill as f64 > 40.0 * pb.frames_per_tick {
            return; // device stalled: don't build a backlog
        }

        self.scratch.clear();
        if prefill {
            self.scratch.resize(pb.frames_per_tick.round() as usize * 2, 0.0);
        }
        pb.resampler.process_interleaved(mix, &mut self.scratch);
        let n = self.scratch.len().min(pb.prod.slots()) & !1;
        if let Ok(chunk) = pb.prod.write_chunk_uninit(n) {
            chunk.fill_from_iter(self.scratch.iter().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sender clock off by `drift`, delivering 10 ms bursts; one minute.
    fn run(drift: f64) -> JitterStats {
        let mut pd = PeerDsp::new();
        let jb = parking_lot::Mutex::new(JitterBuffer::<STEREO_BLOCK>::new(2).with_min_target(4));
        let stats = parking_lot::Mutex::new(JitterStats::default());
        let (mut seq, mut owed, mut t) = (0u32, 0.0f64, 0u64);
        for tick in 0..24_000u64 {
            // Sender: every 4th tick, whatever its clock produced since.
            owed += 1.0 + drift;
            if tick % 4 == 0 {
                while owed >= 1.0 {
                    jb.lock().push(seq, [0.1; STEREO_BLOCK], t);
                    seq += 1;
                    owed -= 1.0;
                }
            }
            t += 2500;
            pd.next_stream_block(&jb, &stats);
        }
        *stats.lock()
    }

    /// The same minute with plain pops: what the stream did before.
    fn run_plain(drift: f64) -> JitterStats {
        let mut jb = JitterBuffer::<STEREO_BLOCK>::new(2).with_min_target(4);
        let (mut seq, mut owed, mut t) = (0u32, 0.0f64, 0u64);
        for tick in 0..24_000u64 {
            owed += 1.0 + drift;
            if tick % 4 == 0 {
                while owed >= 1.0 {
                    jb.push(seq, [0.1; STEREO_BLOCK], t);
                    seq += 1;
                    owed -= 1.0;
                }
            }
            t += 2500;
            jb.pop();
        }
        jb.stats()
    }

    #[test]
    fn plain_buffer_glitches_under_drift() {
        let fast = run_plain(0.001);
        let slow = run_plain(-0.001);
        eprintln!("fast {fast:?}
slow {slow:?}");
        assert!(fast.drops > 10, "{fast:?}");
        assert!(slow.underruns >= 3, "{slow:?}");
    }

    #[test]
    fn stream_rides_out_clock_drift_without_drops_or_gaps() {
        for drift in [0.001, -0.001] {
            let s = run(drift);
            // The first block or two may underrun while the buffer fills.
            assert!(s.underruns <= 2, "drift {drift}: {s:?}");
            assert_eq!(s.drops, 0, "drift {drift}: {s:?}");
        }
    }
}

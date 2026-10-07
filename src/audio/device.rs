//! Audio device I/O on its own thread (cpal streams are not Send everywhere).
//!
//! Two backends on Windows:
//! - **WASAPI** (native, `wasapi.rs`): low-latency shared mode at the
//!   driver's smallest engine period (often 2.7 ms), so other apps keep using
//!   the mic and speakers; optional exclusive mode bypasses the Windows audio
//!   engine altogether. Works with every device, no driver needed.
//! - **ASIO** (cpal): the vendor driver talks to the hardware directly with
//!   buffers as small as the interface allows (32–128 frames ≈ 0.7–2.7 ms).
//!   Input and output run off one clock, so there is no drift between them.
//!
//! Capture is mono (one selected channel, or the average of all). Playback is
//! interleaved stereo written to the first two output channels.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, Thread};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, Stream, StreamConfig, SupportedBufferSize};
use crossbeam_channel::{Receiver, Sender};
use rtrb::{Consumer, Producer, RingBuffer};
use serde::{Deserialize, Serialize};

use crate::protocol::SAMPLE_RATE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Driver {
    #[default]
    Wasapi,
    Asio,
}

impl std::fmt::Display for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Driver::Wasapi => "WASAPI (any device, no driver needed)",
            Driver::Asio => "ASIO (low latency)",
        })
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeviceSpec {
    pub driver: Driver,
    pub input: Option<String>,
    pub output: Option<String>,
    /// ASIO buffer size in frames; `None` = the driver's preferred size.
    pub asio_buffer: Option<u32>,
    /// Zero-based input channel used as the mic; `None` = average all.
    pub mic_channel: Option<u16>,
    /// WASAPI: open devices exclusively (lowest latency, but no other app can
    /// use them meanwhile).
    pub exclusive: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceStatus {
    pub driver: Driver,
    pub input: Option<String>,
    pub output: Option<String>,
    pub input_rate: u32,
    pub output_rate: u32,
    pub input_channels: u16,
    /// Frames per callback actually delivered by the driver.
    pub input_period: usize,
    pub output_period: usize,
    /// How each direction is open, e.g. "exclusive", "low-latency shared".
    pub input_mode: &'static str,
    pub output_mode: &'static str,
    /// ASIO buffer sizes the driver accepts.
    pub buffer_range: Option<(u32, u32)>,
    pub xruns: u64,
    pub error: Option<String>,
}

pub struct DeviceIo {
    pub capture: Option<(Consumer<f32>, u32)>,
    /// Interleaved stereo.
    pub playback: Option<(Producer<f32>, u32)>,
}

pub enum DeviceCmd {
    Rebuild(DeviceSpec),
}

/// Live counters the callbacks update.
pub struct Counters {
    pub input_period: AtomicUsize,
    pub output_period: AtomicUsize,
    pub output_underruns: AtomicU64,
    pub xruns: AtomicU64,
    /// Voice packets the OS refused to send.
    pub send_errors: AtomicU64,
    /// Smallest playback slack (frames left in the ring after a device read)
    /// since the mixer last took it; `usize::MAX` = no reads yet.
    pub output_slack: AtomicUsize,
}

impl Default for Counters {
    fn default() -> Self {
        Self {
            input_period: AtomicUsize::new(0),
            output_period: AtomicUsize::new(0),
            output_underruns: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            send_errors: AtomicU64::new(0),
            output_slack: AtomicUsize::new(usize::MAX),
        }
    }
}

impl Counters {
    /// Called by playback callbacks before they take `frames` stereo frames.
    pub fn note_read(&self, available: usize, frames: usize) {
        self.output_slack.fetch_min(available.saturating_sub(frames), Ordering::Relaxed);
    }
}

/// Buffer sizes tried in "auto" mode, smallest first.
const AUTO_ASIO: [u32; 3] = [64, 128, 256];
/// WASAPI exclusive periods tried in order, as multiples of the device minimum.
const AUTO_EXCLUSIVE: [u32; 3] = [1, 2, 4];

pub fn asio_available() -> bool {
    host(Driver::Asio)
        .ok()
        .and_then(|h| h.devices().ok().map(|mut d| d.next().is_some()))
        .unwrap_or(false)
}

fn host(driver: Driver) -> Result<cpal::Host, String> {
    match driver {
        Driver::Wasapi => Ok(cpal::default_host()),
        #[cfg(windows)]
        Driver::Asio => cpal::host_from_id(cpal::HostId::Asio).map_err(|e| e.to_string()),
        #[cfg(not(windows))]
        Driver::Asio => Err("ASIO is Windows-only".into()),
    }
}

/// (inputs, outputs) for a driver. For ASIO both lists are driver names.
pub fn list(driver: Driver) -> (Vec<String>, Vec<String>) {
    #[cfg(windows)]
    if driver == Driver::Wasapi {
        return super::wasapi::list();
    }
    let Ok(host) = host(driver) else { return Default::default() };
    let names = |it: Result<cpal::InputDevices<cpal::Devices>, cpal::Error>| -> Vec<String> {
        it.map(|d| d.filter_map(|d| device_name(&d)).collect()).unwrap_or_default()
    };
    let inputs = names(host.input_devices());
    let outputs = host
        .output_devices()
        .map(|d| d.filter_map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    (inputs, outputs)
}

pub fn device_name(d: &cpal::Device) -> Option<String> {
    d.description().ok().map(|desc| desc.name().to_string())
}

pub fn spawn(
    spec: DeviceSpec,
    counters: Arc<Counters>,
    wake: Thread,
    status: Arc<parking_lot::Mutex<DeviceStatus>>,
    io_tx: Sender<DeviceIo>,
) -> Sender<DeviceCmd> {
    let (tx, rx) = crossbeam_channel::unbounded();
    thread::Builder::new()
        .name("audio-devices".into())
        .spawn(move || run(spec, rx, counters, wake, status, io_tx))
        .expect("spawn device thread");
    tx
}

/// Set when a stream dies (device unplugged, driver reset).
type Health = AtomicBool;

fn run(
    mut spec: DeviceSpec,
    rx: Receiver<DeviceCmd>,
    counters: Arc<Counters>,
    wake: Thread,
    status: Arc<parking_lot::Mutex<DeviceStatus>>,
    io_tx: Sender<DeviceIo>,
) {
    let mut auto_level = 0usize;
    loop {
        let health = Arc::new(Health::default());
        // No fixed size chosen means "auto": start small, back off on glitches.
        let asio_auto = spec.driver == Driver::Asio && spec.asio_buffer.is_none();
        let exclusive_auto = spec.driver == Driver::Wasapi && spec.exclusive;
        let auto = asio_auto || exclusive_auto;
        let level = auto_level.min(AUTO_ASIO.len() - 1);
        let effective = DeviceSpec {
            asio_buffer: spec.asio_buffer.or(asio_auto.then(|| AUTO_ASIO[level])),
            ..spec.clone()
        };
        let xruns_at_open = counters.xruns.load(Ordering::Relaxed);
        let opened = std::time::Instant::now();
        let mut st = DeviceStatus { driver: spec.driver, ..Default::default() };
        let mut errors = Vec::new();
        let mut streams: Vec<Box<dyn std::any::Any>> = Vec::new();
        let mut io = DeviceIo { capture: None, playback: None };

        #[cfg(windows)]
        let native = spec.driver == Driver::Wasapi;
        #[cfg(not(windows))]
        let native = false;
        if native {
            #[cfg(windows)]
            open_native(&effective, AUTO_EXCLUSIVE[level], &counters, &health, &wake, &mut st, &mut streams, &mut io, &mut errors);
        } else {
            match host(spec.driver) {
                Ok(host) => {
                    // One ASIO driver serves both directions; enumerating again while
                    // it is loaded fails, so open the device once and share it.
                    let shared = (spec.driver == Driver::Asio)
                        .then(|| {
                            find_device(host.input_devices().ok(), spec.input.as_deref())
                                .or_else(|| host.default_input_device())
                        })
                        .flatten();
                    match open_input(&host, &effective, shared.clone(), &counters, &health, &wake) {
                        Ok(o) => {
                            st.input = Some(o.name);
                            st.input_rate = o.rate;
                            st.input_channels = o.channels;
                            st.input_mode = "ASIO";
                            st.buffer_range = o.range;
                            streams.push(Box::new(o.stream));
                            io.capture = Some((o.ring, o.rate));
                        }
                        Err(e) => errors.push(format!("Microphone: {e}")),
                    }
                    match open_output(&host, &effective, shared, &counters, &health) {
                        Ok(o) => {
                            st.output = Some(o.name);
                            st.output_rate = o.rate;
                            st.output_mode = "ASIO";
                            st.buffer_range = st.buffer_range.or(o.range);
                            streams.push(Box::new(o.stream));
                            io.playback = Some((o.ring, o.rate));
                        }
                        Err(e) => errors.push(format!("Speakers: {e}")),
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", spec.driver)),
            }
        }
        st.error = (!errors.is_empty()).then(|| errors.join(" · "));
        *status.lock() = st;
        counters.input_period.store(0, Ordering::Relaxed);
        counters.output_period.store(0, Ordering::Relaxed);
        if io_tx.send(io).is_err() {
            return;
        }
        wake.unpark();

        loop {
            match rx.recv_timeout(Duration::from_millis(250)) {
                Ok(DeviceCmd::Rebuild(new)) => {
                    if new.driver != spec.driver || new.input != spec.input || new.exclusive != spec.exclusive {
                        auto_level = 0;
                    }
                    spec = new;
                    break;
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    let mut s = status.lock();
                    s.input_period = counters.input_period.load(Ordering::Relaxed);
                    s.output_period = counters.output_period.load(Ordering::Relaxed);
                    s.xruns = counters.xruns.load(Ordering::Relaxed);
                    drop(s);
                    let xruns = counters.xruns.load(Ordering::Relaxed) - xruns_at_open;
                    let secs = opened.elapsed().as_secs_f32();
                    if auto && auto_level + 1 < AUTO_ASIO.len() && xruns >= 4 && xruns as f32 / secs.max(1.0) > 0.3 {
                        auto_level += 1;
                        eprintln!("audio: {xruns} glitches, backing off to buffer level {auto_level}");
                        break;
                    }
                    if health.load(Ordering::Relaxed) {
                        // Device unplugged or driver reset: reopen shortly.
                        thread::sleep(Duration::from_secs(1));
                        break;
                    }
                }
            }
        }
        // ASIO drivers must be released before another one is loaded.
        drop(streams);
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
fn open_native(
    spec: &DeviceSpec,
    period_mult: u32,
    counters: &Arc<Counters>,
    health: &Arc<Health>,
    wake: &Thread,
    st: &mut DeviceStatus,
    streams: &mut Vec<Box<dyn std::any::Any>>,
    io: &mut DeviceIo,
    errors: &mut Vec<String>,
) {
    use super::wasapi;
    let (prod, cons) = RingBuffer::<f32>::new(SAMPLE_RATE as usize);
    match wasapi::capture(wasapi::CaptureArgs {
        device: spec.input.clone(),
        mic_channel: spec.mic_channel,
        exclusive: spec.exclusive,
        period_mult,
        prod,
        wake: wake.clone(),
        counters: counters.clone(),
        failed: health.clone(),
    }) {
        Ok((stream, info)) => {
            st.input = Some(info.name);
            st.input_rate = info.rate;
            st.input_channels = info.channels;
            st.input_mode = info.mode.label();
            streams.push(Box::new(stream));
            io.capture = Some((cons, info.rate));
        }
        Err(e) => errors.push(format!("Microphone: {e}")),
    }
    let (prod, cons) = RingBuffer::<f32>::new(SAMPLE_RATE as usize);
    match wasapi::render(wasapi::RenderArgs {
        device: spec.output.clone(),
        exclusive: spec.exclusive,
        period_mult,
        cons,
        counters: counters.clone(),
        failed: health.clone(),
    }) {
        Ok((stream, info)) => {
            st.output = Some(info.name);
            st.output_rate = info.rate;
            st.output_mode = info.mode.label();
            streams.push(Box::new(stream));
            io.playback = Some((prod, info.rate));
        }
        Err(e) => errors.push(format!("Speakers: {e}")),
    }
}

fn error_callback(
    which: &'static str,
    health: &Arc<Health>,
    counters: &Arc<Counters>,
) -> impl FnMut(cpal::Error) + Send + 'static {
    let (h, c) = (health.clone(), counters.clone());
    move |e: cpal::Error| {
        if e.kind() == cpal::ErrorKind::Xrun {
            c.xruns.fetch_add(1, Ordering::Relaxed);
        } else {
            eprintln!("{which} stream error: {e}");
            h.store(true, Ordering::Relaxed);
        }
    }
}

fn find_device(devices: Option<impl Iterator<Item = cpal::Device>>, name: Option<&str>) -> Option<cpal::Device> {
    let name = name?;
    devices?.find(|d| device_name(d).as_deref() == Some(name))
}

/// Prefers 48 kHz (no resampling) in f32/i32/i16, keeping the default channel count.
fn pick_config(
    ranges: Result<impl Iterator<Item = cpal::SupportedStreamConfigRange>, cpal::Error>,
    default: Result<cpal::SupportedStreamConfig, cpal::Error>,
) -> Result<cpal::SupportedStreamConfig, String> {
    let default = default.map_err(|e| e.to_string())?;
    let ranges: Vec<_> = ranges.map(|r| r.collect()).unwrap_or_default();
    for fmt in [SampleFormat::F32, SampleFormat::I32, SampleFormat::I16, default.sample_format()] {
        if let Some(r) = ranges.iter().find(|r| {
            r.sample_format() == fmt
                && r.channels() == default.channels()
                && (r.min_sample_rate()..=r.max_sample_rate()).contains(&SAMPLE_RATE)
        }) {
            return Ok(r.with_sample_rate(SAMPLE_RATE));
        }
    }
    Ok(default)
}

fn buffer_choices(spec: &DeviceSpec, supported: &cpal::SupportedStreamConfig) -> (Vec<cpal::BufferSize>, Option<(u32, u32)>) {
    let range = match supported.buffer_size() {
        SupportedBufferSize::Range { min, max } => Some((*min, *max)),
        SupportedBufferSize::Unknown => None,
    };
    // Only ASIO honours a fixed size; WASAPI shared mode just glitches.
    let fixed = match (spec.driver, spec.asio_buffer, range) {
        (Driver::Asio, Some(n), Some((lo, hi))) => Some(cpal::BufferSize::Fixed(n.clamp(lo, hi))),
        _ => None,
    };
    (fixed.into_iter().chain([cpal::BufferSize::Default]).collect(), range)
}

struct Opened<T> {
    stream: Stream,
    ring: T,
    name: String,
    rate: u32,
    channels: u16,
    range: Option<(u32, u32)>,
}

fn open_input(
    host: &cpal::Host,
    spec: &DeviceSpec,
    preset: Option<cpal::Device>,
    counters: &Arc<Counters>,
    health: &Arc<Health>,
    wake: &Thread,
) -> Result<Opened<Consumer<f32>>, String> {
    let device = preset
        .or_else(|| find_device(host.input_devices().ok(), spec.input.as_deref()))
        .or_else(|| host.default_input_device())
        .ok_or("no input device")?;
    let name = device_name(&device).unwrap_or_else(|| "Microphone".into());
    let supported = pick_config(device.supported_input_configs(), device.default_input_config())?;
    let rate = supported.sample_rate();
    let channels = supported.channels();
    let pick = spec.mic_channel.filter(|&c| c < channels).map(usize::from);
    let (choices, range) = buffer_choices(spec, &supported);

    let mut last_err = String::new();
    for buffer in choices {
        let config = StreamConfig { buffer_size: buffer, ..supported.config() };
        let (prod, cons) = RingBuffer::<f32>::new(rate as usize);
        let args = InputArgs {
            channels: channels as usize,
            pick,
            prod,
            wake: wake.clone(),
            counters: counters.clone(),
        };
        let err = error_callback("input", health, counters);
        let built = match supported.sample_format() {
            SampleFormat::F32 => build_input::<f32>(&device, &config, args, err),
            SampleFormat::I32 => build_input::<i32>(&device, &config, args, err),
            SampleFormat::I16 => build_input::<i16>(&device, &config, args, err),
            SampleFormat::U16 => build_input::<u16>(&device, &config, args, err),
            other => return Err(format!("unsupported sample format {other}")),
        };
        match built.map_err(|e| e.to_string()).and_then(|s| s.play().map(|_| s).map_err(|e| e.to_string())) {
            Ok(stream) => return Ok(Opened { stream, ring: cons, name, rate, channels, range }),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

struct InputArgs {
    channels: usize,
    pick: Option<usize>,
    prod: Producer<f32>,
    wake: Thread,
    counters: Arc<Counters>,
}

fn build_input<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut a: InputArgs,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let inv = 1.0 / a.channels as f32;
    device.build_input_stream::<T, _, _>(
        config.clone(),
        move |data: &[T], _| {
            a.counters.input_period.store(data.len() / a.channels, Ordering::Relaxed);
            for frame in data.chunks_exact(a.channels) {
                let s = match a.pick {
                    Some(c) => f32::from_sample_(frame[c]),
                    None => frame.iter().map(|&x| f32::from_sample_(x)).sum::<f32>() * inv,
                };
                let _ = a.prod.push(s);
            }
            a.wake.unpark();
        },
        err,
        None,
    )
}

fn open_output(
    host: &cpal::Host,
    spec: &DeviceSpec,
    preset: Option<cpal::Device>,
    counters: &Arc<Counters>,
    health: &Arc<Health>,
) -> Result<Opened<Producer<f32>>, String> {
    let device = preset
        .or_else(|| find_device(host.output_devices().ok(), spec.output.as_deref()))
        .or_else(|| host.default_output_device())
        .ok_or("no output device")?;
    let name = device_name(&device).unwrap_or_else(|| "Speakers".into());
    let supported = pick_config(device.supported_output_configs(), device.default_output_config())?;
    let rate = supported.sample_rate();
    let channels = supported.channels();
    let (choices, range) = buffer_choices(spec, &supported);

    let mut last_err = String::new();
    for buffer in choices {
        let config = StreamConfig { buffer_size: buffer, ..supported.config() };
        let (prod, cons) = RingBuffer::<f32>::new(rate as usize);
        let err = error_callback("output", health, counters);
        let c = counters.clone();
        let ch = channels as usize;
        let built = match supported.sample_format() {
            SampleFormat::F32 => build_output::<f32>(&device, &config, ch, cons, c, err),
            SampleFormat::I32 => build_output::<i32>(&device, &config, ch, cons, c, err),
            SampleFormat::I16 => build_output::<i16>(&device, &config, ch, cons, c, err),
            SampleFormat::U16 => build_output::<u16>(&device, &config, ch, cons, c, err),
            other => return Err(format!("unsupported sample format {other}")),
        };
        match built.map_err(|e| e.to_string()).and_then(|s| s.play().map(|_| s).map_err(|e| e.to_string())) {
            Ok(stream) => return Ok(Opened { stream, ring: prod, name, rate, channels, range }),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn build_output<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    mut cons: Consumer<f32>,
    counters: Arc<Counters>,
    err: impl FnMut(cpal::Error) + Send + 'static,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    device.build_output_stream::<T, _, _>(
        config.clone(),
        move |data: &mut [T], _| {
            counters.output_period.store(data.len() / channels, Ordering::Relaxed);
            counters.note_read(cons.slots() / 2, data.len() / channels);
            let mut starved = false;
            for frame in data.chunks_exact_mut(channels) {
                let (l, r) = if cons.slots() >= 2 {
                    (cons.pop().unwrap_or(0.0), cons.pop().unwrap_or(0.0))
                } else {
                    starved = true;
                    (0.0, 0.0)
                };
                if channels == 1 {
                    frame[0] = T::from_sample_((l + r) * 0.5);
                } else {
                    frame[0] = T::from_sample_(l);
                    frame[1] = T::from_sample_(r);
                    for s in &mut frame[2..] {
                        *s = T::from_sample_(0.0f32);
                    }
                }
            }
            if starved {
                counters.output_underruns.fetch_add(1, Ordering::Relaxed);
            }
        },
        err,
        None,
    )
}

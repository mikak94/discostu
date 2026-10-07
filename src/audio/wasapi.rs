//! Native WASAPI at the lowest latency Windows offers without a vendor driver.
//!
//! - **Exclusive mode** skips the Windows audio engine entirely (no mixer, no
//!   "enhancements"/APOs, no resampling) and runs event-driven at the
//!   device's minimum period, typically 1–3 ms. The device is reserved for us
//!   while it is open.
//! - **Low-latency shared mode** (`IAudioClient3`) asks the engine for the
//!   smallest period the driver supports (often 128 frames = 2.7 ms with the
//!   in-box HD Audio and USB Audio 2 drivers) while other apps keep playing.
//!
//! Low-latency shared is the default for both directions, so games and other
//! apps can still use the mic and speakers. Exclusive is opt-in and falls
//! back to shared if another app already holds the device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use rtrb::{Consumer, Producer};
use windows::Win32::Foundation::{CloseHandle, HANDLE, PROPERTYKEY, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED,
    AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, DEVICE_STATE_ACTIVE,
    EDataFlow, IAudioCaptureClient, IAudioClient, IAudioClient3, IAudioRenderClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVEFORMATEXTENSIBLE_0, eCapture, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, STGM_READ,
};
use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::{GUID, w};

use super::device::Counters;

const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY =
    PROPERTYKEY { fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0), pid: 14 };
const SUBTYPE_PCM: GUID = GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);
const SUBTYPE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xfffe;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Exclusive,
    LowLatency,
    Shared,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Exclusive => "exclusive",
            Mode::LowLatency => "low-latency shared",
            Mode::Shared => "shared",
        }
    }
}

/// An open stream; dropping it stops the device thread.
pub struct Stream {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct Info {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub mode: Mode,
}

/// Friendly names of active (capture, render) endpoints.
pub fn list() -> (Vec<String>, Vec<String>) {
    with_com(|| unsafe {
        let Ok(e) = enumerator() else { return Default::default() };
        (names(&e, eCapture), names(&e, eRender))
    })
}

/// Runs `f` on a fresh MTA thread. Initialising COM on the caller's thread
/// would pin its apartment, and ASIO drivers refuse to load from an MTA
/// thread — listing WASAPI devices first used to make ASIO vanish.
fn with_com<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    thread::scope(|s| {
        s.spawn(|| {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            f()
        })
        .join()
        .unwrap_or_else(|e| std::panic::resume_unwind(e))
    })
}

/// One line per endpoint: name, mix format, and the periods each mode offers.
pub fn report() -> Vec<String> {
    with_com(report_inner)
}

fn report_inner() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let Ok(e) = enumerator() else { return out };
        for (flow, kind) in [(eCapture, "mic"), (eRender, "out")] {
            let Ok(c) = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { continue };
            for i in 0..c.GetCount().unwrap_or(0) {
                let Ok(d) = c.Item(i) else { continue };
                let name = friendly_name(&d).unwrap_or_default();
                let mut line = format!("{kind} {name}:");
                if let Ok(client) = d.Activate::<IAudioClient3>(CLSCTX_ALL, None)
                    && let Ok(mix) = client.GetMixFormat()
                {
                    if let Some(f) = Format::parse(mix) {
                        line += &format!(" {} Hz {} ch {:?}", f.rate, f.channels, f.sample);
                    }
                    let (mut def, mut fund, mut min, mut max) = (0, 0, 0, 0);
                    if client.GetSharedModeEnginePeriod(mix, &mut def, &mut fund, &mut min, &mut max).is_ok() {
                        line += &format!(" · shared period {min}–{def} frames");
                    }
                    CoTaskMemFree(Some(mix as *const _));
                    let (mut def, mut min) = (0i64, 0i64);
                    if client.GetDevicePeriod(Some(&mut def), Some(&mut min)).is_ok() {
                        line += &format!(" · exclusive min {:.2} ms", min as f64 / 10_000.0);
                    }
                    if let Some(f) = exclusive_format(&client) {
                        line += &format!(" ({:?} {} ch {} Hz)", f.sample, f.channels, f.rate);
                    }
                }
                out.push(line);
            }
        }
    }
    out
}

unsafe fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

unsafe fn names(e: &IMMDeviceEnumerator, flow: EDataFlow) -> Vec<String> {
    unsafe {
        let Ok(c) = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else { return Vec::new() };
        (0..c.GetCount().unwrap_or(0)).filter_map(|i| c.Item(i).ok()).filter_map(|d| friendly_name(&d)).collect()
    }
}

unsafe fn friendly_name(d: &IMMDevice) -> Option<String> {
    unsafe {
        let store = d.OpenPropertyStore(STGM_READ).ok()?;
        let value = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
        Some(value.to_string())
    }
}

unsafe fn find(flow: EDataFlow, name: Option<&str>) -> Result<(IMMDevice, String), String> {
    unsafe {
        let e = enumerator().map_err(|e| e.to_string())?;
        if let Some(name) = name
            && let Ok(c) = e.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
        {
            for i in 0..c.GetCount().unwrap_or(0) {
                if let Ok(d) = c.Item(i)
                    && friendly_name(&d).as_deref() == Some(name)
                {
                    return Ok((d, name.to_string()));
                }
            }
        }
        let d = e.GetDefaultAudioEndpoint(flow, eConsole).map_err(|_| "no device")?;
        let name = friendly_name(&d).unwrap_or_else(|| "Default device".into());
        Ok((d, name))
    }
}

// ---------------------------------------------------------------- formats

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    F32,
    I32,
    I24,
    I16,
}

impl Sample {
    fn bytes(self) -> usize {
        match self {
            Sample::F32 | Sample::I32 => 4,
            Sample::I24 => 3,
            Sample::I16 => 2,
        }
    }

    unsafe fn read(self, p: *const u8) -> f32 {
        unsafe {
            match self {
                Sample::F32 => (p as *const f32).read_unaligned(),
                Sample::I32 => (p as *const i32).read_unaligned() as f32 / 2_147_483_648.0,
                Sample::I24 => {
                    let v = (*p as i32) << 8 | (*p.add(1) as i32) << 16 | (*p.add(2) as i32) << 24;
                    v as f32 / 2_147_483_648.0
                }
                Sample::I16 => (p as *const i16).read_unaligned() as f32 / 32_768.0,
            }
        }
    }

    unsafe fn write(self, p: *mut u8, v: f32) {
        let v = v.clamp(-1.0, 1.0);
        unsafe {
            match self {
                Sample::F32 => (p as *mut f32).write_unaligned(v),
                Sample::I32 => (p as *mut i32).write_unaligned((v as f64 * 2_147_483_647.0) as i32),
                Sample::I24 => {
                    let i = (v * 8_388_607.0) as i32;
                    *p = i as u8;
                    *p.add(1) = (i >> 8) as u8;
                    *p.add(2) = (i >> 16) as u8;
                }
                Sample::I16 => (p as *mut i16).write_unaligned((v * 32_767.0) as i16),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Format {
    sample: Sample,
    channels: u16,
    rate: u32,
}

impl Format {
    fn frame_bytes(&self) -> usize {
        self.sample.bytes() * self.channels as usize
    }

    fn wave(&self) -> WAVEFORMATEXTENSIBLE {
        let (bits, valid, sub) = match self.sample {
            Sample::F32 => (32, 32, SUBTYPE_FLOAT),
            Sample::I32 => (32, 24, SUBTYPE_PCM), // 24-in-32, the common native USB/HDA format
            Sample::I24 => (24, 24, SUBTYPE_PCM),
            Sample::I16 => (16, 16, SUBTYPE_PCM),
        };
        let align = self.frame_bytes() as u16;
        WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_EXTENSIBLE,
                nChannels: self.channels,
                nSamplesPerSec: self.rate,
                nAvgBytesPerSec: self.rate * align as u32,
                nBlockAlign: align,
                wBitsPerSample: bits,
                cbSize: 22,
            },
            Samples: WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: valid },
            dwChannelMask: match self.channels {
                1 => 0x4, // front centre
                2 => 0x3, // front left | right
                _ => 0,
            },
            SubFormat: sub,
        }
    }

    unsafe fn parse(p: *const WAVEFORMATEX) -> Option<Format> {
        unsafe {
            let f = p.read_unaligned();
            let (float, bits) = match f.wFormatTag {
                WAVE_FORMAT_IEEE_FLOAT => (true, f.wBitsPerSample),
                WAVE_FORMAT_PCM => (false, f.wBitsPerSample),
                WAVE_FORMAT_EXTENSIBLE => {
                    let x = (p as *const WAVEFORMATEXTENSIBLE).read_unaligned();
                    let sub = x.SubFormat;
                    (sub == SUBTYPE_FLOAT, f.wBitsPerSample)
                }
                _ => return None,
            };
            let sample = match (float, bits) {
                (true, 32) => Sample::F32,
                (false, 32) => Sample::I32,
                (false, 24) => Sample::I24,
                (false, 16) => Sample::I16,
                _ => return None,
            };
            Some(Format { sample, channels: f.nChannels, rate: f.nSamplesPerSec })
        }
    }
}

// ---------------------------------------------------------------- opening

struct Client {
    client: IAudioClient,
    format: Format,
    /// Frames per device period (= per event).
    period: u32,
    mode: Mode,
}

/// The best native format the device accepts exclusively: 48 kHz first, then
/// its mix rate; float, 24-in-32, packed 24, 16-bit.
unsafe fn exclusive_format(client: &IAudioClient) -> Option<Format> {
    unsafe {
        let mix = client.GetMixFormat().ok()?;
        let mix_fmt = Format::parse(mix);
        CoTaskMemFree(Some(mix as *const _));
        let mut channels = vec![mix_fmt.map_or(2, |f| f.channels), 2, 1];
        channels.dedup();
        let mut rates = vec![48_000, mix_fmt.map_or(48_000, |f| f.rate), 44_100];
        rates.dedup();
        for &rate in &rates {
            for &ch in &channels {
                for sample in [Sample::F32, Sample::I32, Sample::I24, Sample::I16] {
                    let f = Format { sample, channels: ch, rate };
                    let wave = f.wave();
                    if client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &wave.Format, None).0 == 0 {
                        return Some(f);
                    }
                }
            }
        }
        None
    }
}

/// Exclusive mode at `mult` × the device's minimum period.
unsafe fn open_exclusive(device: &IMMDevice, mult: u32) -> Result<Client, String> {
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let format = exclusive_format(&client).ok_or("device offers no exclusive PCM format")?;
        eprintln!("wasapi: exclusive format {:?} {} ch {} Hz", format.sample, format.channels, format.rate);

        let (mut default, mut min) = (0i64, 0i64);
        client.GetDevicePeriod(Some(&mut default), Some(&mut min)).map_err(|e| e.to_string())?;
        let mut period = (min * mult as i64).min(default.max(min));
        let wave = format.wave();
        let mut client = client;
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        if let Err(e) = client.Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, period, period, &wave.Format, None) {
            if e.code() != AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
                return Err(exclusive_error(e));
            }
            // The driver wants a period that is a whole number of its blocks.
            let frames = client.GetBufferSize().map_err(|e| e.to_string())?;
            period = (10_000_000.0 * frames as f64 / format.rate as f64 + 0.5) as i64;
            client = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
            client
                .Initialize(AUDCLNT_SHAREMODE_EXCLUSIVE, flags, period, period, &wave.Format, None)
                .map_err(exclusive_error)?;
        }
        let period = client.GetBufferSize().map_err(|e| e.to_string())?;
        Ok(Client { client, format, period, mode: Mode::Exclusive })
    }
}

fn exclusive_error(e: windows::core::Error) -> String {
    match e.code().0 as u32 {
        0x8889000A => "device is in use by another app".into(),
        0x8889000E => "exclusive mode is disabled for this device (Sound settings → Advanced)".into(),
        _ => e.message(),
    }
}

/// Shared mode at the engine's smallest period, or its default if the
/// driver does not support smaller ones.
unsafe fn open_shared(device: &IMMDevice) -> Result<Client, String> {
    unsafe {
        let client: IAudioClient3 = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
        let mix = client.GetMixFormat().map_err(|e| e.to_string())?;
        let result = (|| {
            let format = Format::parse(mix).ok_or("unsupported mix format")?;
            let (mut default, mut fundamental, mut min, mut max) = (0, 0, 0, 0);
            client
                .GetSharedModeEnginePeriod(mix, &mut default, &mut fundamental, &mut min, &mut max)
                .map_err(|e| e.to_string())?;
            eprintln!("wasapi: engine periods default {default} min {min} max {max} frames @ {} Hz", format.rate);
            let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
            if min < default && client.InitializeSharedAudioStream(flags, min, mix, None).is_ok() {
                // The engine runs one period per device: if another app already
                // started it at the default, we get that instead.
                let (mut fmt, mut current) = (std::ptr::null_mut(), min);
                if client.GetCurrentSharedModeEnginePeriod(&mut fmt, &mut current).is_ok() {
                    CoTaskMemFree(Some(fmt as *const _));
                }
                let mode = if current < default { Mode::LowLatency } else { Mode::Shared };
                return Ok(Client { client: client.clone().into(), format, period: current, mode });
            }
            // Smaller periods unsupported: plain shared mode on a fresh client.
            let plain: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(|e| e.to_string())?;
            plain.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, mix, None).map_err(|e| e.message())?;
            Ok(Client { client: plain, format, period: default, mode: Mode::Shared })
        })();
        CoTaskMemFree(Some(mix as *const _));
        result
    }
}

// ---------------------------------------------------------------- streams

pub struct CaptureArgs {
    pub device: Option<String>,
    pub mic_channel: Option<u16>,
    pub exclusive: bool,
    /// Exclusive period as a multiple of the device minimum (auto backoff).
    pub period_mult: u32,
    pub prod: Producer<f32>,
    pub wake: Thread,
    pub counters: Arc<Counters>,
    pub failed: Arc<AtomicBool>,
}

pub struct RenderArgs {
    pub device: Option<String>,
    pub exclusive: bool,
    pub period_mult: u32,
    pub cons: Consumer<f32>,
    pub counters: Arc<Counters>,
    pub failed: Arc<AtomicBool>,
}

pub fn capture(a: CaptureArgs) -> Result<(Stream, Info), String> {
    spawn("audio-capture", move |stop, ready| unsafe { run_capture(a, stop, ready) })
}

pub fn render(a: RenderArgs) -> Result<(Stream, Info), String> {
    spawn("audio-render", move |stop, ready| unsafe { run_render(a, stop, ready) })
}

type Ready = std::sync::mpsc::SyncSender<Result<Info, String>>;

fn spawn(name: &str, body: impl FnOnce(&AtomicBool, &Ready) + Send + 'static) -> Result<(Stream, Info), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let s = stop.clone();
    let thread = thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                // MMCSS "Pro Audio": real-time scheduling class for audio threads.
                let mut task = 0u32;
                let _ = AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task);
            }
            body(&s, &tx)
        })
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(info)) => Ok((Stream { stop, thread: Some(thread) }, info)),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err("device did not start".into())
        }
    }
}

unsafe fn event(client: &IAudioClient) -> Result<HANDLE, String> {
    unsafe {
        let ev = CreateEventW(None, false, false, None).map_err(|e| e.to_string())?;
        client.SetEventHandle(ev).map_err(|e| e.to_string())?;
        Ok(ev)
    }
}

unsafe fn run_capture(mut a: CaptureArgs, stop: &AtomicBool, ready: &Ready) {
    unsafe {
        let opened = (|| {
            let (device, name) = find(eCapture, a.device.as_deref())?;
            let c = match a.exclusive.then(|| open_exclusive(&device, a.period_mult)) {
                Some(Ok(c)) => c,
                other => {
                    if let Some(Err(e)) = other {
                        eprintln!("mic exclusive mode unavailable ({e}); using shared");
                    }
                    open_shared(&device)?
                }
            };
            let capture: IAudioCaptureClient = c.client.GetService().map_err(|e| e.to_string())?;
            let ev = event(&c.client)?;
            c.client.Start().map_err(|e| e.to_string())?;
            Ok::<_, String>((c, capture, ev, name))
        })();
        let (c, capture, ev, name) = match opened {
            Ok(o) => o,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let f = c.format;
        let _ = ready.send(Ok(Info { name, rate: f.rate, channels: f.channels, mode: c.mode }));

        let ch = f.channels as usize;
        let step = f.sample.bytes();
        let frame_bytes = f.frame_bytes();
        let pick = a.mic_channel.map(usize::from).filter(|&p| p < ch);
        let inv = 1.0 / ch as f32;
        let mut first = true;
        'run: while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(ev, 200) != WAIT_OBJECT_0 {
                continue;
            }
            loop {
                match capture.GetNextPacketSize() {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break 'run,
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                if capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break 'run;
                }
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && !first {
                    a.counters.xruns.fetch_add(1, Ordering::Relaxed);
                }
                first = false;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null();
                for i in 0..frames as usize {
                    let s = if silent {
                        0.0
                    } else {
                        let base = data.add(i * frame_bytes);
                        match pick {
                            Some(p) => f.sample.read(base.add(p * step)),
                            None => (0..ch).map(|c| f.sample.read(base.add(c * step))).sum::<f32>() * inv,
                        }
                    };
                    let _ = a.prod.push(s);
                }
                let _ = capture.ReleaseBuffer(frames);
                a.counters.input_period.store(frames as usize, Ordering::Relaxed);
            }
            a.wake.unpark();
        }
        if !stop.load(Ordering::Relaxed) {
            eprintln!("microphone stream lost");
            a.failed.store(true, Ordering::Relaxed);
        }
        let _ = c.client.Stop();
        let _ = CloseHandle(ev);
    }
}

unsafe fn run_render(mut a: RenderArgs, stop: &AtomicBool, ready: &Ready) {
    unsafe {
        let opened = (|| {
            let (device, name) = find(eRender, a.device.as_deref())?;
            let c = match a.exclusive.then(|| open_exclusive(&device, a.period_mult)) {
                Some(Ok(c)) => c,
                other => {
                    if let Some(Err(e)) = other {
                        eprintln!("speaker exclusive mode unavailable ({e}); using shared");
                    }
                    open_shared(&device)?
                }
            };
            let render: IAudioRenderClient = c.client.GetService().map_err(|e| e.to_string())?;
            let ev = event(&c.client)?;
            let buffer = c.client.GetBufferSize().map_err(|e| e.to_string())?;
            // Exclusive: the driver plays one buffer while we fill the other;
            // start with one of silence queued.
            if c.mode == Mode::Exclusive {
                render.GetBuffer(buffer).map_err(|e| e.to_string())?;
                render.ReleaseBuffer(buffer, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32).map_err(|e| e.to_string())?;
            }
            c.client.Start().map_err(|e| e.to_string())?;
            Ok::<_, String>((c, render, ev, name, buffer))
        })();
        let (c, render, ev, name, buffer) = match opened {
            Ok(o) => o,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        let f = c.format;
        let _ = ready.send(Ok(Info { name, rate: f.rate, channels: f.channels, mode: c.mode }));

        let ch = f.channels as usize;
        let step = f.sample.bytes();
        let frame_bytes = f.frame_bytes();
        // Shared mode: the engine takes one period per pass and signals us
        // right after, so one period plus a small wakeup margin is enough.
        let margin = (c.period / 4).max(f.rate / 400); // ≥ 2.5 ms
        let target = (c.period + margin).min(buffer);
        'run: while !stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(ev, 200) != WAIT_OBJECT_0 {
                continue;
            }
            let frames = if c.mode == Mode::Exclusive {
                buffer
            } else {
                match c.client.GetCurrentPadding() {
                    Ok(pad) => target.saturating_sub(pad),
                    Err(_) => break 'run,
                }
            };
            if frames == 0 {
                continue;
            }
            let Ok(data) = render.GetBuffer(frames) else { break 'run };
            a.counters.note_read(a.cons.slots() / 2, frames as usize);
            let mut starved = false;
            for i in 0..frames as usize {
                let (l, r) = if a.cons.slots() >= 2 {
                    (a.cons.pop().unwrap_or(0.0), a.cons.pop().unwrap_or(0.0))
                } else {
                    starved = true;
                    (0.0, 0.0)
                };
                let base = data.add(i * frame_bytes);
                if ch == 1 {
                    f.sample.write(base, (l + r) * 0.5);
                } else {
                    f.sample.write(base, l);
                    f.sample.write(base.add(step), r);
                    for c in 2..ch {
                        f.sample.write(base.add(c * step), 0.0);
                    }
                }
            }
            if render.ReleaseBuffer(frames, 0).is_err() {
                break 'run;
            }
            if starved {
                a.counters.output_underruns.fetch_add(1, Ordering::Relaxed);
            }
            a.counters.output_period.store(c.period as usize, Ordering::Relaxed);
        }
        if !stop.load(Ordering::Relaxed) {
            eprintln!("speaker stream lost");
            a.failed.store(true, Ordering::Relaxed);
        }
        let _ = c.client.Stop();
        let _ = CloseHandle(ev);
    }
}

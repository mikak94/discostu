# discostu

Peer-to-peer LAN voice chat and screen sharing for Windows, in Rust, with an
[Iced](https://iced.rs) UI. No servers: peers find each other by UDP
broadcast and connect directly.

```
cargo run --release
```

Building needs LLVM (for the ASIO bindings; `.cargo/config.toml` points
`LIBCLANG_PATH` at `C:/Program Files/LLVM/bin`). The asio-sys build script
downloads Steinberg's ASIO SDK on first build, and `build.rs` embeds the app
icon with the Windows SDK's `rc.exe`.

Named after Disco Stu. The icon and palette (lavender shirt, pink flames, gold
medallion) are his; the artwork is © 20th Television, so keep builds private.
`assets/disco-stu.ai` is a fan vector tracing; `cargo run --example make_art`
converts it to SVG and regenerates the badge, `icon.png` and `discostu.ico`.

## Channels

Everyone on the LAN starts in the **Lobby**. Anyone can create a channel; the
creator hosts it:

- It's saved in the creator's config and opens whenever they're online.
- When the creator goes offline it disappears, and its members drop back to
  the Lobby. It comes back when they return.
- You only hear and send voice to people in your channel. Audio still flows
  directly between members, never through the host, so channels add no latency.

There's no server: each peer's status (sent to every other peer over its
control connection) lists the channels it hosts and the channel it's in.

## Audio

- **ASIO** (default when a hardware ASIO driver is installed): buffers start
  at 64 frames (1.3 ms) and step up only if the driver glitches. Input and
  output share one clock. Pick the mic channel on multi-input interfaces.
- **WASAPI** (native, no driver needed) works with any device. Shared mode
  asks Windows for the smallest engine period the driver supports (2.7 ms on
  Microsoft's in-box drivers, 10 ms on many vendor drivers) and keeps only one
  period plus 2.5 ms queued; games and other apps can still use the mic.
  Optional **exclusive mode** bypasses the Windows audio engine for ~3 ms
  periods, but locks other apps out of the devices.
  `discostu --audio-devices` lists each device's supported periods.
- ASIO wrappers (FL Studio ASIO, ASIO4ALL, FlexASIO) sit on top of WASAPI and
  are never faster than native WASAPI.
- Raw 48 kHz PCM in 2.5 ms UDP packets (no codec delay). Adaptive jitter
  buffer down to 1 block. Clock drift is absorbed by ±0.3% resampling, never
  by dropping samples.

**Voice processing** (`src/audio`, one 2.5 ms tick): echo cancellation
(NLMS with GCC-PHAT delay estimation, two-filter double-talk protection,
crossfaded bypass), a voice-profile gate with 250 ms hold, same-room
separation (levels tell who is talking: a roommate talking alone closes your
outgoing mic so their voice never comes back to them, and your voice profile
only learns from speech that is yours; incoming streams are never modified), and a soft limiter instead of hard clipping. Same-room
friends always stay audible in your headphones.

## Screen sharing

Windows Graphics Capture (a whole screen or a single window, cursor
included) → D3D11 video processor BGRA→NV12 on the GPU → **hardware H.264**
through Media Foundation (NVENC / AMF / Quick Sync) in low-latency mode, no
B-frames. Decoded with DXVA (software fallback, or force it with
`DISCOSTU_SW_DECODE=1`) and converted NV12→RGB in a wgpu shader.

Each viewer has its own short queue. A viewer that falls behind drops its
backlog and resyncs on a fresh keyframe, so it never adds lag for anyone.

**Share audio**: per-process loopback. Sharing a screen sends every app's
sound *except discostu's* (no echo of the call). Sharing a window sends only
that app's sound.

If no hardware encoder exists, screen shares fall back to a lossless tile codec.

## Testing on one machine

Set `DISCOSTU_ISOLATED=1` on test instances: they skip LAN discovery and only
dial `manual_peers`, so they never join anyone's real session. ASIO drivers
allow one client at a time, so give the second instance WASAPI:

```
DISCOSTU_PROFILE=a discostu --headless --share
DISCOSTU_PROFILE=b discostu --headless --watch            # set audio_driver "Wasapi" in config-b.json
discostu --headless --share-window "visual studio code"
DISCOSTU_PROFILE=a discostu --headless --create-channel "Disco night"
DISCOSTU_PROFILE=b discostu --headless --join-any
discostu --audio-devices                                   # periods, formats, ASIO drivers
```

Peers only talk to the same protocol version (currently 2), so update every
machine together.

`cargo test` covers the DSP, the resampler, the jitter buffer, the codec and
the wire formats.

# discostu

Peer-to-peer voice chat and screen sharing for Windows, in Rust, with an
[Iced](https://iced.rs) UI. Peers on a LAN find each other by UDP broadcast;
friends elsewhere meet through a small broker. Either way, voice and video go
directly between the two machines, encrypted, never through a server.

```
app/       the Windows app (cargo run --release)
proto/     wire formats, identities, QUIC/TLS setup: shared, cross-platform
broker/    the internet meeting point: Linux, Docker, Fly.io
```

Building the app needs LLVM (for the ASIO bindings; `.cargo/config.toml`
points `LIBCLANG_PATH` at `C:/Program Files/LLVM/bin`). The asio-sys build
script downloads Steinberg's ASIO SDK on first build, and `app/build.rs`
embeds the app icon with the Windows SDK's `rc.exe`.

Named after Disco Stu. The icon and palette (lavender shirt, pink flames, gold
medallion) are his; the artwork is © 20th Television, so keep builds private.
`app/assets/disco-stu.ai` is a fan vector tracing; `cargo run --example
make_art` converts it to SVG and regenerates the badge, `icon.png` and
`discostu.ico`.

## Connections

Everything runs over **QUIC** (quinn) on one UDP port (47802):

- One connection per peer: a control stream (status, channel) plus voice and
  screen-share audio as unreliable datagrams.
- Watching a screen opens a second connection to the same peer, so video has
  its own congestion control and can never delay voice.
- TLS 1.3 always. Each install has a self-signed certificate made on first
  run (`identity.bin` in the config folder); its SHA-256 fingerprint is the
  peer id, so ids can't be faked.

**Groups.** A friends group is a random code (`XXXX-XXXX-XXXX-XXXX`, 80 bits)
made in Settings → Internet and sent to friends. Peers prove they know it with
an HMAC over the TLS session's exported keying material, which ties the proof
to that one connection: nobody can replay it or sit in the middle, the broker
included. Without a code you're in the open LAN group, which works like
before: everyone on the network.

**Finding each other:**

1. **LAN**: broadcast beacons on port 47800 carry a group tag; peers with the
   same group dial each other.
2. **Internet**: the app keeps a QUIC connection to the broker from the same
   UDP socket. The broker tells every group member the others' addresses:
   the public address it sees each at, their LAN addresses (for friends
   behind the same router), global IPv6 addresses, and a router port mapping
   if one exists.
3. Both sides then dial every address at once. Each side's outgoing packets
   open its own router for the other: **UDP hole punching**. Works for most
   home connections.
4. **Port mapping**: on start the app asks the router to forward its port
   (UPnP, then PCP/NAT-PMP). That makes it reachable even when the other side
   is behind a strict NAT. The mapping is renewed while the app runs and
   removed on exit. Behind carrier-grade NAT it can't help, and the app says so.
5. If both sides are behind strict NATs with no mapping, they can't connect:
   the broker never relays. The sidebar lists them as "connecting…".

`manual_peers` (Settings → Network) still dials `host[:port]` directly.

## Channels

Everyone starts in the **Lobby**. Channels belong to the group and are kept
by the broker (on disk, so they survive restarts):

- A channel is open while its creator is online; when they go offline it
  disappears and its members drop back to the Lobby. It returns with them.
- Only the creator can delete it.
- You only hear and send voice to people in your channel. Audio flows directly
  between members, so channels add no latency.

Channels need a group and a broker connection.

## Broker

`broker/` is a small tokio + quinn server: presence, address exchange, and
channels per group, nothing else. It never sees group codes (only a hash),
never relays media, and can't impersonate peers. Its own certificate is
self-signed; apps pin its fingerprint on first connect (Settings → Internet
has "Trust new key" for when you redeploy with a new identity).

Configuration: `BROKER_BIND` (default `0.0.0.0:47900`), `BROKER_DATA`
(identity key and `channels.json`, default `./broker-data`).

```
cargo run -p discostu-broker                      # locally
fly deploy . --config broker/fly.toml --dockerfile broker/Dockerfile
```

Pushes to `master` that touch `broker/` or `proto/` are tested and deployed
by `.github/workflows/broker.yml` (needs the `FLY_API_TOKEN` repository
secret: `fly tokens create deploy -a discostu-broker`).

On Fly.io, UDP needs a dedicated IPv4 (`fly ips allocate-v4`) and binds
`fly-global-services`; the identity and channels live on a volume
(`fly volumes create broker_data -s 1`). The app's default broker is
`discostu-broker.fly.dev`.

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
- Raw 48 kHz PCM in 2.5 ms QUIC datagrams (no codec delay). Adaptive jitter
  buffer down to 1 block. Clock drift is absorbed by ±0.3% resampling, never
  by dropping samples.

**Voice processing** (`app/src/audio`, one 2.5 ms tick): echo cancellation
(NLMS with GCC-PHAT delay estimation, two-filter double-talk protection,
crossfaded bypass), a voice-profile gate with 250 ms hold, same-room
separation (levels tell who is talking: a roommate talking alone closes your
outgoing mic so their voice never comes back to them, and your voice profile
only learns from speech that is yours; incoming streams are never modified),
and a soft limiter instead of hard clipping. Same-room friends always stay
audible in your headphones.

## Screen sharing

Windows Graphics Capture (a whole screen or a single window, cursor
included) → D3D11 video processor BGRA→NV12 on the GPU → **hardware H.264**
through Media Foundation (NVENC / AMF / Quick Sync) in low-latency mode, no
B-frames. Decoded with DXVA (software fallback, or force it with
`DISCOSTU_SW_DECODE=1`) and converted NV12→RGB in a wgpu shader.

Each viewer has its own short queue, and the QUIC send window is kept small
(1 MB). A viewer that falls behind drops its backlog and resyncs on a fresh
keyframe, so it never adds lag for anyone.

**Share audio**: per-process loopback. Sharing a screen sends every app's
sound *except discostu's* (no echo of the call). Sharing a window sends only
that app's sound.

If no hardware encoder exists, screen shares fall back to a lossless tile codec.

## Testing on one machine

Set `DISCOSTU_ISOLATED=1` on test instances: they skip LAN discovery and
router port mapping (`DISCOSTU_UPNP=1` turns mapping back on), so they never
join anyone's real session. Give each its own `DISCOSTU_PROFILE` and a group
code nobody else uses. ASIO drivers allow one client at a time, so give the
second instance WASAPI:

```
cargo run -p discostu-broker                       # BROKER_BIND=127.0.0.1:47900
DISCOSTU_PROFILE=a discostu --headless --broker 127.0.0.1 --group TEST-TEST-TEST-TEST --create-channel jam --share
DISCOSTU_PROFILE=b discostu --headless --broker 127.0.0.1 --group TEST-TEST-TEST-TEST --join-any --watch
discostu --headless --share-window "visual studio code"
discostu --audio-devices                           # periods, formats, ASIO drivers
```

Peers only talk to the same protocol version (currently 3, part of the QUIC
ALPN), so update every machine together.

`cargo test --workspace` covers the DSP, the resampler, the jitter buffer,
the codec, the wire formats and the group proofs.

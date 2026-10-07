//! `discostu --headless [--share | --share-window <title>] [--watch] [--seconds N]
//!   [--create-channel <name>] [--join-any] [--diagnostics <after seconds>] [--stop-share-after <seconds>]
//!   [--group <code> | --new-group | --no-group] [--broker <host[:port]> | --no-broker]`
//!
//! Runs the engine without a window and prints a status line per second.
//! Handy for diagnosing networks and for testing two instances on one
//! machine (give each its own `DISCOSTU_PROFILE`).

use std::time::{Duration, Instant};

use crate::engine::Engine;
use crate::protocol;
use crate::screen::{self, SourceKind};

pub fn run(args: &[String]) {
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let seconds: Option<u64> = value("--seconds").and_then(|s| s.parse().ok());

    let engine = match Engine::start() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to start: {e}");
            std::process::exit(1);
        }
    };
    if let Some(b) = value("--broker") {
        engine.set_broker(&b);
    }
    if flag("--no-broker") {
        engine.set_broker("");
    }
    if let Some(code) = value("--group") {
        engine.set_group_code(&code);
    }
    if flag("--new-group") {
        println!("new group {}", engine.new_group());
    }
    if flag("--no-group") {
        engine.set_group_code("");
    }
    let snap = engine.snapshot();
    println!(
        "discostu {} ({:016x}) udp {} · {} · group {} · broker {}",
        snap.me.name,
        engine.me,
        engine.net.port,
        snap.addresses.join(", "),
        if snap.internet.group_code.is_empty() { "-" } else { &snap.internet.group_code },
        if snap.internet.broker.is_empty() { "off" } else { &snap.internet.broker },
    );
    let mut create = value("--create-channel");

    let wanted_window = value("--share-window").map(|s| s.to_lowercase());
    if flag("--share") || wanted_window.is_some() {
        let sources = screen::sources();
        let pick = match &wanted_window {
            Some(w) => sources.iter().find(|s| s.is_window() && s.title.to_lowercase().contains(w)),
            None => sources.iter().find(|s| matches!(s.kind, SourceKind::Monitor { .. })),
        };
        match pick {
            Some(s) => match engine.start_share(s) {
                Ok(warning) => {
                    println!("sharing {} ({})", s.title, s.subtitle);
                    if let Some(w) = warning {
                        println!("note: {w}");
                    }
                }
                Err(e) => println!("share failed: {e}"),
            },
            None => println!("nothing to share matched"),
        }
    }

    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let s = engine.snapshot();
        let d = &s.devices;
        let mut line = format!(
            "[{:>3}s] {:?} in {}@{} ({}) out {}@{} ({}) xruns {} underruns {} send errors {} dsp {:.0}% lvl {:.3}",
            start.elapsed().as_secs(),
            d.driver,
            d.input.as_deref().map_or("-", |_| "ok"),
            d.input_period,
            d.input_mode,
            d.output.as_deref().map_or("-", |_| "ok"),
            d.output_period,
            d.output_mode,
            d.xruns,
            s.playback_underruns,
            s.send_errors,
            s.dsp_load * 100.0,
            s.me.level,
        );
        if let Some(h) = &s.me.share {
            line += &format!(
                " | sharing {} fps {:.1} Mb/s → {} viewer(s) [{}{}]",
                h.fps,
                h.mbps,
                h.viewers,
                h.codec,
                if h.audio { " + audio" } else { "" }
            );
        }
        line += &format!(" | broker {:?} upnp {:?}", s.internet.status, s.internet.portmap);
        for w in &s.internet.waiting {
            line += &format!(" | waiting for {}", w.name);
        }
        for p in &s.peers {
            line += &format!(
                " | {} ({} {}) rtt {:.3} ms jitter {:.2} ms buf {} | net rx {} lost {} reord {} gaps>10/20/50ms {}/{}/{} max {:.1} ms | jb gap {} underrun {} late {}{}{}",
                p.name,
                p.path,
                p.addr,
                p.rtt_us as f32 / 1000.0,
                p.jitter.jitter_us / 1000.0,
                p.jitter.target,
                p.net.received,
                p.net.expected - p.net.received,
                p.net.reordered,
                p.net.gaps_10ms,
                p.net.gaps_20ms,
                p.net.gaps_50ms,
                p.net.max_gap_us as f32 / 1000.0,
                p.jitter.lost,
                p.jitter.underruns,
                p.jitter.late,
                if p.sharing.is_some() { " [sharing]" } else { "" },
                if p.streaming { " [audio]" } else { "" },
            );
        }
        for p in s.peers.iter().filter(|p| p.stream_net.received > 0) {
            line += &format!(
                " | {} stream rx {} lost {} gaps>20ms {} max {:.1} ms buf {}/{} underrun {} drops {}",
                p.name,
                p.stream_net.received,
                p.stream_net.expected - p.stream_net.received,
                p.stream_net.gaps_20ms,
                p.stream_net.max_gap_us as f32 / 1000.0,
                p.stream_jitter.target,
                p.stream_jitter.jitter_us as u32 / 1000,
                p.stream_jitter.underruns,
                p.stream_jitter.drops,
            );
        }
        if let Some(v) = &s.video {
            line += &format!(
                " | video {} {}x{} {} fps e2e {:.1} ms (enc {:.1} dec {:.1}) {:.1} Mb/s",
                v.codec, v.width, v.height, v.fps, v.latency_ms, v.encode_ms, v.decode_ms, v.mbps
            );
        }
        if let Some(e) = &d.error {
            line += &format!(" | audio: {e}");
        }
        let channel_name = |at: Option<protocol::ChannelRef>| match at {
            None => "lobby".to_string(),
            Some(at) => s.channels.iter().find(|c| c.at == at).map_or("?".into(), |c| c.name.clone()),
        };
        line += &format!(" | in {}", channel_name(s.channel));
        for p in &s.peers {
            line += &format!(" | {} in {}", p.name, channel_name(p.channel));
        }
        if let Some(n) = engine.take_notice() {
            line += &format!(" | notice: {n}");
        }
        println!("{line}");

        if flag("--watch")
            && s.watching.is_none()
            && let Some(p) = s.peers.iter().find(|p| p.sharing.is_some())
        {
            match engine.watch(p.id) {
                Ok(()) => println!("watching {}", p.name),
                Err(e) => println!("watch failed: {e}"),
            }
        }
        if let Some(name) = create.as_deref()
            && matches!(s.internet.status, crate::engine::BrokerStatus::Connected { .. })
        {
            match engine.create_channel(name) {
                Ok(()) => println!("created channel {name}"),
                Err(e) => println!("create channel failed: {e}"),
            }
            create = None;
        }
        if flag("--join-any")
            && s.channel.is_none()
            && let Some(c) = s.channels.iter().find(|c| !c.mine)
        {
            match engine.join(Some(c.at)) {
                Ok(()) => println!("joined {} (hosted by {})", c.name, c.host),
                Err(e) => println!("join failed: {e}"),
            }
        }
        if value("--stop-share-after").and_then(|s| s.parse::<u64>().ok()) == Some(start.elapsed().as_secs()) {
            engine.stop_share();
            println!("stopped sharing");
        }
        if value("--diagnostics").and_then(|s| s.parse::<u64>().ok()) == Some(start.elapsed().as_secs()) {
            engine.save_diagnostics();
        }
        if let Some(r) = engine.take_diagnostics_result() {
            println!("diagnostics: {r:?}");
        }
        if seconds.is_some_and(|n| start.elapsed().as_secs() >= n) {
            break;
        }
    }
    engine.shutdown();
}

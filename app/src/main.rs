//! discostu — peer-to-peer voice chat and screen sharing, on a LAN or over
//! the internet (a small broker introduces peers; media never passes it).
//!
//! Threads (plain OS threads unless noted; the UI runs on iced's executor):
//! - `net` (tokio)     QUIC endpoint: peer connections, voice datagrams,
//!                     broker client, router port mapping
//! - `net::discovery`  UDP broadcast beacons, finds peers on the LAN
//! - `net::media`      clock pings
//! - `audio` device    owns the cpal streams
//! - `audio` dsp       2.5 ms tick: AEC, gate, voice profile, mixing, crosstalk cancel
//! - `screen` capture  grabs frames, diffs tiles, feeds per-viewer writers

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod clock;
mod config;
mod engine;
mod headless;
mod net;
mod protocol;
mod screen;
mod ui;

fn main() -> iced::Result {
    clock::init();
    let args: Vec<String> = std::env::args().collect();
    #[cfg(windows)]
    if args.iter().any(|a| a == "--audio-devices") {
        for line in audio::wasapi::report() {
            println!("{line}");
        }
        for name in audio::device::list(audio::Driver::Asio).0 {
            println!("asio {name}");
        }
        return Ok(());
    }
    if args.iter().any(|a| a == "--headless") {
        headless::run(&args);
        return Ok(());
    }
    let result = ui::run();
    engine::shutdown_running();
    result
}

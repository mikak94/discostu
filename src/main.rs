//! discostu — peer-to-peer LAN voice chat and screen sharing.
//!
//! Threads (all plain OS threads; the UI runs on iced's executor):
//! - `net::discovery`  UDP broadcast beacons, finds peers on the LAN
//! - `net::listener`   TCP accept loop for control + screen connections
//! - `net::media`      UDP receive loop: audio packets and pings
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
    ui::run()
}

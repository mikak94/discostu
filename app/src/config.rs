//! Persistent user settings (JSON in the platform config dir).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::audio::Driver;
use crate::protocol::{Channel, PeerId};

/// The public broker this build connects to unless told otherwise.
pub const DEFAULT_BROKER: &str = "discostu-broker.fly.dev";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    /// `None` until first run picks ASIO when a hardware driver exists.
    pub audio_driver: Option<Driver>,
    pub input_device: Option<String>,
    pub output_device: Option<String>,
    /// ASIO buffer in frames; `None` = driver preference.
    pub asio_buffer: Option<u32>,
    /// Zero-based mic input channel; `None` = average all inputs.
    pub mic_channel: Option<u16>,
    /// WASAPI exclusive mode (lowest latency; blocks other apps from the devices).
    pub exclusive_audio: bool,
    /// Volume of screen-share audio from others.
    pub stream_volume: f32,
    /// Send system/app audio along with screen shares.
    pub share_audio: bool,
    /// Cancel our own speaker output from our microphone.
    pub echo_cancel: bool,
    /// Cancel our own voice when it leaks into other peers' microphones.
    pub crosstalk_cancel: bool,
    /// Voice-profile driven noise gate on the microphone.
    pub noise_gate: bool,
    /// Friends group code (normalized). Empty: no group, LAN only.
    pub group_code: String,
    /// `host[:port]` of the broker; empty turns internet connections off.
    pub broker: String,
    /// Broker address → certificate fingerprint seen on first connect.
    pub broker_pins: HashMap<String, String>,
    /// `host[:port]` entries to connect to when broadcast discovery is blocked.
    pub manual_peers: Vec<String>,
    /// Channels from before they lived on the broker; handed over on the
    /// first broker connection, then empty.
    pub channels: Vec<Channel>,
    pub peer_volumes: HashMap<PeerId, f32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: default_name(),
            audio_driver: None,
            input_device: None,
            output_device: None,
            asio_buffer: None,
            mic_channel: None,
            exclusive_audio: false,
            stream_volume: 1.0,
            share_audio: true,
            echo_cancel: true,
            crosstalk_cancel: true,
            noise_gate: true,
            group_code: String::new(),
            broker: DEFAULT_BROKER.into(),
            broker_pins: HashMap::new(),
            manual_peers: Vec::new(),
            channels: Vec::new(),
            peer_volumes: HashMap::new(),
        }
    }
}

impl Config {
    pub fn dir() -> PathBuf {
        directories::ProjectDirs::from("dev", "discostu", "discostu")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn path() -> PathBuf {
        // DISCOSTU_PROFILE lets several instances run on one machine for testing.
        match std::env::var("DISCOSTU_PROFILE") {
            Ok(p) if !p.is_empty() => Self::dir().join(format!("config-{p}.json")),
            _ => Self::dir().join("config.json"),
        }
    }

    pub fn load() -> Self {
        let cfg = std::fs::read(Self::path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
            .unwrap_or_default();
        cfg.save();
        cfg
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }
}

fn default_name() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "stu".into())
}

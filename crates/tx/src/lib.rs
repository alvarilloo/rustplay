pub mod audio;
pub mod clock;
pub mod conn;
pub mod event;
pub mod info;
mod mirror;
pub mod mirror_setup;
pub mod pacing;
pub mod pairing;
pub mod rtp;
mod seal;
mod session;
pub mod setup;
pub mod video;
pub mod video_thread;

use std::collections::HashMap;
use std::time::Duration;

use tokio::time::Instant;

use rustplay_core::mdns;
pub use rustplay_core::mdns::DiscoveredDevice;
use rustplay_core::Result;

pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let (airplay, raop) = tokio::join!(
        collect(rustplay_core::AIRPLAY_SERVICE, timeout),
        collect(rustplay_core::RAOP_SERVICE, timeout)
    );
    let (airplay, raop) = match (airplay, raop) {
        (Err(e), Err(_)) => return Err(e),
        (a, r) => (a.unwrap_or_default(), r.unwrap_or_default()),
    };

    let mut merged: HashMap<String, DiscoveredDevice> = HashMap::new();
    for device in airplay {
        merged.insert(normalized_id(&device), device);
    }
    for device in raop {
        merged.entry(normalized_id(&device)).or_insert(device);
    }
    let mut devices: Vec<_> = merged.into_values().collect();
    devices.sort_by(|a, b| {
        a.display_name()
            .to_lowercase()
            .cmp(&b.display_name().to_lowercase())
            .then_with(|| a.instance.cmp(&b.instance))
    });
    Ok(devices)
}

async fn collect(service: &'static str, timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
    let mut browser = mdns::browse(service)?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<DiscoveredDevice> = Vec::new();
    while let Ok(Some(device)) = tokio::time::timeout_at(deadline, browser.next()).await {
        match found.iter_mut().find(|d| d.instance == device.instance) {
            Some(existing) => *existing = device,
            None => found.push(device),
        }
    }
    Ok(found)
}

fn normalized_id(device: &DiscoveredDevice) -> String {
    let raw = match device.device_id() {
        Some(id) => id,
        None => device
            .instance
            .split_once('@')
            .map_or(device.instance.as_str(), |(mac, _)| mac),
    };
    raw.chars()
        .filter(char::is_ascii_hexdigit)
        .collect::<String>()
        .to_ascii_uppercase()
}

#[derive(Debug, Clone, Copy)]
pub struct AudioOptions {
    pub latency_ms: u32,
    pub volume_db: Option<f32>,
}

impl Default for AudioOptions {
    fn default() -> Self {
        Self {
            latency_ms: 2000,
            volume_db: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct MirrorOptions {
    pub monitor: u32,
    // Shares a single window (HWND) instead of the monitor.
    pub window: Option<isize>,
    pub fps: u32,
    pub bitrate: u32,
    pub audio: bool,
    pub volume_db: Option<f32>,
    // Caps the encoded height (keeping the aspect ratio); None uses the receiver's size.
    pub max_height: Option<u32>,
    pub cursor: bool,
    // Playout lead of the mirrored audio; None uses the default (170 ms).
    pub audio_latency_ms: Option<u32>,
}

impl Default for MirrorOptions {
    fn default() -> Self {
        Self {
            monitor: 0,
            window: None,
            fps: 30,
            bitrate: 8_000_000,
            audio: true,
            volume_db: None,
            max_height: None,
            cursor: true,
            audio_latency_ms: None,
        }
    }
}

pub use session::{Session, StreamHandle};

use std::path::PathBuf;

use plist::{Dictionary, Value};

pub const QUALITY_BITRATES: [u32; 4] = [4_000_000, 8_000_000, 15_000_000, 25_000_000];
pub const QUALITY_KEYS: [&str; 4] = ["quality.low", "quality.balanced", "quality.high", "quality.max"];
pub const RESOLUTION_CAPS: [Option<u32>; 3] = [None, Some(1080), Some(720)];
pub const RESOLUTION_KEYS: [&str; 3] = ["resolution.auto", "1080p", "720p"];
pub const LATENCY_MS: [Option<u32>; 4] = [None, Some(250), Some(500), Some(1000)];
pub const LATENCY_KEYS: [&str; 4] = ["latency.auto", "250 ms", "500 ms", "latency.second"];
pub const RECV_SIZES: [(u32, u32); 3] = [(1920, 1080), (2560, 1440), (3840, 2160)];
pub const RECV_SIZE_NAMES: [&str; 3] = ["1080p", "1440p", "4K"];
pub const SCALING_KEYS: [&str; 3] = ["receive.settings.scaling.fit", "receive.settings.scaling.fill", "receive.settings.scaling.stretch"];
pub const KEEP_KEYS: [&str; 4] = [
    "receive.settings.keep.off",
    "receive.settings.keep.1min",
    "receive.settings.keep.5min",
    "receive.settings.keep.always",
];
pub fn keep_window(index: usize) -> rustplay_rx::KeepWindow {
    use rustplay_rx::KeepWindow;
    match index {
        0 => KeepWindow::Off,
        1 => KeepWindow::For(std::time::Duration::from_secs(60)),
        2 => KeepWindow::For(std::time::Duration::from_secs(300)),
        _ => KeepWindow::Always,
    }
}
pub const ACCESS_KEYS: [&str; 3] = ["receive.access.everyone", "receive.access.allowed_only", "receive.access.ask"];
pub const THEME_KEYS: [&str; 3] = ["settings.theme.system", "settings.theme.light", "settings.theme.dark"];
pub const ACCENT_KEYS: [&str; 2] = ["settings.accent.brand", "settings.accent.windows"];
// Language names are always shown in their own language.
pub const LANGUAGE_KEYS: [&str; 3] = ["settings.language.system", "Español", "English"];

pub const AUDIO_DELAY_MAX_MS: i32 = rustplay_rx::AUDIO_DELAY_MAX_MS;
pub const AUDIO_DELAY_STEP_MS: i32 = 10;

// Rounds to the nearest step inside ±AUDIO_DELAY_MAX_MS.
pub fn clamp_delay(ms: i64) -> i32 {
    let max = i64::from(AUDIO_DELAY_MAX_MS);
    let step = i64::from(AUDIO_DELAY_STEP_MS);
    let ms = ms.clamp(-max, max);
    ((ms + ms.signum() * step / 2) / step * step) as i32
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceState {
    // Seen, but with no decision yet.
    Known,
    Allowed,
    Blocked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KnownDevice {
    // Upper-case device id as sent by the iPhone (a MAC-like string).
    pub id: String,
    pub name: String,
    pub model: String,
    pub state: DeviceState,
}

// A device marked as favourite; the name and kind are the last ones seen, to list it while it is offline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Favorite {
    pub key: String,
    pub name: String,
    pub speaker: bool,
}

pub const ALIAS_MAX_CHARS: usize = 40;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub monitor: u32,
    // Share a single window instead of the whole monitor.
    pub share_window: bool,
    // The chosen window is remembered by title and process, since handles do not survive a restart.
    pub window_title: String,
    pub window_process: String,
    pub auto_reconnect: bool,
    pub remember_device: bool,
    pub last_device: String,
    pub connect_on_start: bool,
    pub fps: u32,
    pub quality: usize,
    pub resolution: usize,
    pub cursor: bool,
    pub audio: bool,
    pub audio_latency: usize,
    pub receiver_name: String,
    pub receiver_audio: bool,
    pub receiver_enabled: bool,
    pub receiver_size: usize,
    pub receiver_fps: u32,
    pub receiver_fullscreen: bool,
    pub receiver_on_top: bool,
    pub receiver_backdrop: bool,
    pub receiver_scaling: usize,
    pub receiver_vsync: bool,
    pub receiver_hq_scaling: bool,
    pub receiver_controls: bool,
    pub receiver_stats: bool,
    pub receiver_borderless: bool,
    pub receiver_blur: bool,
    pub receiver_lock: bool,
    // WASAPI endpoint ID of the speakers for received audio; empty follows the system default.
    pub receiver_output: String,
    // Lip-sync offset for received audio, a multiple of AUDIO_DELAY_STEP_MS.
    pub receiver_audio_delay: i32,
    pub receiver_multi: bool,
    pub receiver_keep: usize,
    pub receiver_pin: bool,
    pub access_policy: usize,
    pub known_devices: Vec<KnownDevice>,
    // Empty means the default folder (Videos\RustPlay, Pictures\RustPlay).
    pub recordings_dir: String,
    pub screenshots_dir: String,
    pub record_audio: bool,
    pub receiver_app_videos: bool,
    pub receiver_app_photos: bool,
    pub theme: usize,
    // 0: RustPlay orange, 1: the Windows accent colour.
    pub accent: usize,
    pub language: usize,
    pub close_to_tray: bool,
    pub auto_discover: bool,
    pub developer_logs: bool,
    pub hide_ips: bool,
    pub notifications: bool,
    pub notify_connections: bool,
    pub notify_errors: bool,
    pub update_check: bool,
    // The first-run setup assistant has been shown.
    pub onboarding_done: bool,
    // Run the downloaded installer silently (progress only) instead of showing its wizard.
    pub update_silent: bool,
    // Newest version already announced with a notification, so each release is announced once.
    pub update_notified: String,
    pub favorites: Vec<Favorite>,
    // Custom names by device key.
    pub aliases: std::collections::BTreeMap<String, String>,
    pub save_history: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            monitor: 0,
            share_window: false,
            window_title: String::new(),
            window_process: String::new(),
            auto_reconnect: true,
            remember_device: true,
            last_device: String::new(),
            connect_on_start: false,
            fps: 30,
            quality: 1,
            resolution: 0,
            cursor: true,
            audio: true,
            audio_latency: 0,
            receiver_name: std::env::var("COMPUTERNAME")
                .ok()
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| String::from("RustPlay")),
            receiver_audio: true,
            receiver_enabled: false,
            receiver_size: 0,
            receiver_fps: 60,
            receiver_fullscreen: false,
            receiver_on_top: false,
            receiver_backdrop: true,
            receiver_scaling: 0,
            receiver_vsync: false,
            receiver_hq_scaling: false,
            receiver_controls: true,
            receiver_stats: false,
            receiver_borderless: false,
            receiver_blur: true,
            receiver_lock: false,
            receiver_output: String::new(),
            receiver_audio_delay: 0,
            receiver_multi: false,
            receiver_keep: 2,
            receiver_pin: false,
            access_policy: 0,
            known_devices: Vec::new(),
            recordings_dir: String::new(),
            screenshots_dir: String::new(),
            record_audio: true,
            receiver_app_videos: false,
            receiver_app_photos: false,
            theme: 0,
            accent: 0,
            language: 0,
            close_to_tray: false,
            auto_discover: true,
            developer_logs: false,
            hide_ips: false,
            notifications: true,
            notify_connections: true,
            notify_errors: true,
            update_check: true,
            onboarding_done: false,
            update_silent: true,
            update_notified: String::new(),
            favorites: Vec::new(),
            aliases: std::collections::BTreeMap::new(),
            save_history: true,
        }
    }
}

fn folder_or(custom: &str, default: fn() -> PathBuf) -> PathBuf {
    if custom.trim().is_empty() { default() } else { PathBuf::from(custom) }
}

fn path() -> Option<PathBuf> {
    rustplay_core::identity::data_dir().ok().map(|dir| dir.join("rustplay.plist"))
}

fn read_device(d: &Dictionary) -> Option<KnownDevice> {
    let text = |k: &str| d.get(k).and_then(Value::as_string).map(str::to_owned);
    let id = text("id").filter(|id| !id.is_empty())?;
    let state = match d.get("state").and_then(Value::as_unsigned_integer) {
        Some(1) => DeviceState::Allowed,
        Some(2) => DeviceState::Blocked,
        _ => DeviceState::Known,
    };
    Some(KnownDevice { id, name: text("name").unwrap_or_default(), model: text("model").unwrap_or_default(), state })
}

fn write_device(device: &KnownDevice) -> Value {
    let mut d = Dictionary::new();
    d.insert("id".into(), Value::from(device.id.clone()));
    d.insert("name".into(), Value::from(device.name.clone()));
    d.insert("model".into(), Value::from(device.model.clone()));
    let state = match device.state {
        DeviceState::Known => 0u64,
        DeviceState::Allowed => 1,
        DeviceState::Blocked => 2,
    };
    d.insert("state".into(), Value::from(state));
    Value::Dictionary(d)
}

fn read_favorite(d: &Dictionary) -> Option<Favorite> {
    let text = |k: &str| d.get(k).and_then(Value::as_string).map(str::to_owned);
    let key = text("key").filter(|k| !k.is_empty())?;
    Some(Favorite { key, name: text("name").unwrap_or_default(), speaker: d.get("speaker").and_then(Value::as_boolean).unwrap_or(false) })
}

fn write_favorite(f: &Favorite) -> Value {
    let mut d = Dictionary::new();
    d.insert("key".into(), Value::from(f.key.clone()));
    d.insert("name".into(), Value::from(f.name.clone()));
    d.insert("speaker".into(), Value::from(f.speaker));
    Value::Dictionary(d)
}

impl Settings {
    pub fn is_favorite(&self, key: &str) -> bool {
        self.favorites.iter().any(|f| f.key == key)
    }

    // The custom name of a device, when it has one.
    pub fn alias(&self, key: &str) -> Option<&str> {
        self.aliases.get(key).map(String::as_str).filter(|a| !a.is_empty())
    }

    // Sets or (with blank text) removes a custom name.
    pub fn set_alias(&mut self, key: &str, alias: &str) {
        let alias: String = alias.trim().chars().take(ALIAS_MAX_CHARS).collect();
        if alias.is_empty() {
            self.aliases.remove(key);
        } else {
            self.aliases.insert(key.to_owned(), alias);
        }
    }

    pub fn devices_in(&self, state: DeviceState) -> Vec<String> {
        self.known_devices.iter().filter(|d| d.state == state).map(|d| d.id.clone()).collect()
    }

    pub fn bitrate(&self) -> u32 {
        let base = QUALITY_BITRATES[self.quality.min(QUALITY_BITRATES.len() - 1)];
        if self.fps >= 60 { base / 2 * 3 } else { base }
    }

    pub fn recordings_path(&self) -> PathBuf {
        folder_or(&self.recordings_dir, rustplay_core::media::recorder::default_recordings_dir)
    }

    pub fn screenshots_path(&self) -> PathBuf {
        folder_or(&self.screenshots_dir, rustplay_core::media::recorder::default_screenshots_dir)
    }

    pub fn load() -> Self {
        let mut s = Self::default();
        let Some(dict) = path().and_then(|p| Value::from_file(p).ok()).and_then(Value::into_dictionary) else {
            return s;
        };
        let int = |key: &str| dict.get(key).and_then(Value::as_unsigned_integer);
        let index = |key: &str, len: usize| int(key).map(|v| v as usize).filter(|v| *v < len);
        let flag = |key: &str| dict.get(key).and_then(Value::as_boolean);
        let fps = |key: &str| int(key).map(|v| v as u32).filter(|v| matches!(v, 30 | 60));
        s.monitor = int("monitor").map_or(s.monitor, |v| v as u32);
        s.share_window = flag("shareWindow").unwrap_or(s.share_window);
        let text = |key: &str| dict.get(key).and_then(Value::as_string).map(str::to_owned);
        s.window_title = text("windowTitle").unwrap_or_default();
        s.window_process = text("windowProcess").unwrap_or_default();
        s.auto_reconnect = flag("autoReconnect").unwrap_or(s.auto_reconnect);
        s.remember_device = flag("rememberDevice").unwrap_or(s.remember_device);
        s.last_device = text("lastDevice").filter(|_| s.remember_device).unwrap_or_default();
        s.connect_on_start = flag("connectOnStart").unwrap_or(s.connect_on_start);
        s.fps = fps("fps").unwrap_or(s.fps);
        s.quality = index("quality", QUALITY_KEYS.len()).unwrap_or(s.quality);
        s.resolution = index("resolution", RESOLUTION_KEYS.len()).unwrap_or(s.resolution);
        s.cursor = flag("cursor").unwrap_or(s.cursor);
        s.audio = flag("audio").unwrap_or(s.audio);
        s.audio_latency = index("audioLatency", LATENCY_KEYS.len()).unwrap_or(s.audio_latency);
        if let Some(v) = dict.get("receiverName").and_then(Value::as_string).filter(|v| !v.trim().is_empty()) {
            s.receiver_name = v.to_owned();
        }
        s.receiver_audio = flag("receiverAudio").unwrap_or(s.receiver_audio);
        s.receiver_enabled = flag("receiverEnabled").unwrap_or(s.receiver_enabled);
        s.receiver_size = index("receiverSize", RECV_SIZES.len()).unwrap_or(s.receiver_size);
        s.receiver_fps = fps("receiverFps").unwrap_or(s.receiver_fps);
        s.receiver_fullscreen = flag("receiverFullscreen").unwrap_or(s.receiver_fullscreen);
        s.receiver_on_top = flag("receiverOnTop").unwrap_or(s.receiver_on_top);
        s.receiver_backdrop = flag("receiverBackdrop").unwrap_or(s.receiver_backdrop);
        s.receiver_scaling = index("receiverScaling", SCALING_KEYS.len()).unwrap_or(s.receiver_scaling);
        s.receiver_vsync = flag("receiverVsync").unwrap_or(s.receiver_vsync);
        s.receiver_hq_scaling = flag("receiverHqScaling").unwrap_or(s.receiver_hq_scaling);
        s.receiver_controls = flag("receiverControls").unwrap_or(s.receiver_controls);
        s.receiver_stats = flag("receiverStats").unwrap_or(s.receiver_stats);
        s.receiver_borderless = flag("receiverBorderless").unwrap_or(s.receiver_borderless);
        s.receiver_blur = flag("receiverBlur").unwrap_or(s.receiver_blur);
        s.receiver_lock = flag("receiverLock").unwrap_or(s.receiver_lock);
        if let Some(v) = dict.get("receiverOutput").and_then(Value::as_string) {
            s.receiver_output = v.to_owned();
        }
        if let Some(v) = dict.get("receiverAudioDelay").and_then(Value::as_signed_integer) {
            s.receiver_audio_delay = clamp_delay(v);
        }
        s.receiver_multi = flag("receiverMulti").unwrap_or(s.receiver_multi);
        s.receiver_keep = index("receiverKeep", KEEP_KEYS.len()).unwrap_or(s.receiver_keep);
        s.receiver_pin = flag("receiverPin").unwrap_or(s.receiver_pin);
        s.access_policy = index("accessPolicy", ACCESS_KEYS.len()).unwrap_or(s.access_policy);
        if let Some(list) = dict.get("knownDevices").and_then(Value::as_array) {
            s.known_devices = list.iter().filter_map(Value::as_dictionary).filter_map(read_device).collect();
        }
        s.record_audio = flag("recordAudio").unwrap_or(s.record_audio);
        let text = |key: &str| dict.get(key).and_then(Value::as_string).map(str::to_owned);
        s.recordings_dir = text("recordingsDir").unwrap_or(s.recordings_dir);
        s.screenshots_dir = text("screenshotsDir").unwrap_or(s.screenshots_dir);
        s.receiver_app_videos = flag("receiverAppVideos").unwrap_or(s.receiver_app_videos);
        s.receiver_app_photos = flag("receiverAppPhotos").unwrap_or(s.receiver_app_photos);
        s.theme = index("theme", THEME_KEYS.len()).unwrap_or(s.theme);
        s.accent = index("accent", ACCENT_KEYS.len()).unwrap_or(s.accent);
        s.language = index("language", LANGUAGE_KEYS.len()).unwrap_or(s.language);
        s.close_to_tray = flag("closeToTray").unwrap_or(s.close_to_tray);
        s.auto_discover = flag("autoDiscover").unwrap_or(s.auto_discover);
        s.developer_logs = flag("developerLogs").unwrap_or(s.developer_logs);
        s.hide_ips = flag("hideIps").unwrap_or(s.hide_ips);
        s.notifications = flag("notifications").unwrap_or(s.notifications);
        s.notify_connections = flag("notifyConnections").unwrap_or(s.notify_connections);
        s.notify_errors = flag("notifyErrors").unwrap_or(s.notify_errors);
        s.update_check = flag("updateCheck").unwrap_or(s.update_check);
        // A settings file from before the assistant existed belongs to an existing user.
        s.onboarding_done = flag("onboardingDone").unwrap_or(true);
        s.update_silent = flag("updateSilent").unwrap_or(s.update_silent);
        if let Some(v) = dict.get("updateNotified").and_then(Value::as_string) {
            s.update_notified = v.to_owned();
        }
        if let Some(list) = dict.get("favoriteDevices").and_then(Value::as_array) {
            s.favorites = list.iter().filter_map(Value::as_dictionary).filter_map(read_favorite).collect();
        }
        if let Some(map) = dict.get("deviceAliases").and_then(Value::as_dictionary) {
            for (key, value) in map {
                if let Some(alias) = value.as_string() {
                    s.set_alias(key, alias);
                }
            }
        }
        s.save_history = flag("saveHistory").unwrap_or(s.save_history);
        s
    }

    pub fn save(&self) {
        if crate::preview::active() {
            return;
        }
        let Some(path) = path() else { return };
        let mut d = Dictionary::new();
        let mut int = |k: &str, v: u64| {
            d.insert(k.into(), Value::from(v));
        };
        int("monitor", u64::from(self.monitor));
        int("fps", u64::from(self.fps));
        int("quality", self.quality as u64);
        int("resolution", self.resolution as u64);
        int("audioLatency", self.audio_latency as u64);
        int("receiverSize", self.receiver_size as u64);
        int("receiverFps", u64::from(self.receiver_fps));
        int("receiverScaling", self.receiver_scaling as u64);
        int("receiverKeep", self.receiver_keep as u64);
        int("theme", self.theme as u64);
        int("accent", self.accent as u64);
        int("language", self.language as u64);
        for (k, v) in [
            ("shareWindow", self.share_window),
            ("autoReconnect", self.auto_reconnect),
            ("rememberDevice", self.remember_device),
            ("connectOnStart", self.connect_on_start),
            ("cursor", self.cursor),
            ("audio", self.audio),
            ("receiverAudio", self.receiver_audio),
            ("receiverEnabled", self.receiver_enabled),
            ("receiverFullscreen", self.receiver_fullscreen),
            ("receiverOnTop", self.receiver_on_top),
            ("receiverBackdrop", self.receiver_backdrop),
            ("receiverVsync", self.receiver_vsync),
            ("receiverHqScaling", self.receiver_hq_scaling),
            ("receiverControls", self.receiver_controls),
            ("receiverStats", self.receiver_stats),
            ("receiverBorderless", self.receiver_borderless),
            ("receiverBlur", self.receiver_blur),
            ("receiverLock", self.receiver_lock),
            ("receiverMulti", self.receiver_multi),
            ("receiverPin", self.receiver_pin),
            ("recordAudio", self.record_audio),
            ("receiverAppVideos", self.receiver_app_videos),
            ("receiverAppPhotos", self.receiver_app_photos),
            ("closeToTray", self.close_to_tray),
            ("autoDiscover", self.auto_discover),
            ("developerLogs", self.developer_logs),
            ("hideIps", self.hide_ips),
            ("notifications", self.notifications),
            ("notifyConnections", self.notify_connections),
            ("notifyErrors", self.notify_errors),
            ("updateCheck", self.update_check),
            ("saveHistory", self.save_history),
            ("onboardingDone", self.onboarding_done),
            ("updateSilent", self.update_silent),
        ] {
            d.insert(k.into(), Value::from(v));
        }
        d.insert("updateNotified".into(), Value::from(self.update_notified.clone()));
        d.insert("accessPolicy".into(), Value::from(self.access_policy as u64));
        d.insert("knownDevices".into(), Value::Array(self.known_devices.iter().map(write_device).collect()));
        d.insert("favoriteDevices".into(), Value::Array(self.favorites.iter().map(write_favorite).collect()));
        let aliases = self.aliases.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect();
        d.insert("deviceAliases".into(), Value::Dictionary(aliases));
        d.insert("receiverName".into(), Value::from(self.receiver_name.clone()));
        d.insert("receiverOutput".into(), Value::from(self.receiver_output.clone()));
        d.insert("receiverAudioDelay".into(), Value::Integer(i64::from(self.receiver_audio_delay).into()));
        d.insert("recordingsDir".into(), Value::from(self.recordings_dir.clone()));
        d.insert("screenshotsDir".into(), Value::from(self.screenshots_dir.clone()));
        d.insert("windowTitle".into(), Value::from(self.window_title.clone()));
        d.insert("windowProcess".into(), Value::from(self.window_process.clone()));
        let last = if self.remember_device { self.last_device.clone() } else { String::new() };
        d.insert("lastDevice".into(), Value::from(last));
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = Value::Dictionary(d).to_file_xml(&path) {
            tracing::warn!("no se pudo guardar la configuración: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_are_trimmed_capped_and_removable() {
        let mut s = Settings::default();
        s.set_alias("AA", "  Salón  ");
        assert_eq!(s.alias("AA"), Some("Salón"));
        s.set_alias("AA", &"x".repeat(100));
        assert_eq!(s.alias("AA").map(|a| a.chars().count()), Some(ALIAS_MAX_CHARS));
        s.set_alias("AA", "   ");
        assert_eq!(s.alias("AA"), None);
    }

    #[test]
    fn favorite_round_trips_through_plist_values() {
        let f = Favorite { key: "AA:BB".into(), name: "Apple TV".into(), speaker: true };
        let Value::Dictionary(d) = write_favorite(&f) else { panic!() };
        assert_eq!(read_favorite(&d), Some(f));
    }
}

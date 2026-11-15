mod extras;
mod favorites;
mod history_page;
mod access;
mod keyboard;
mod splash;
mod logs;
mod now_playing;
mod pages;
mod setup;
mod welcome;
mod widgets;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Instant;

use rustplay_core::mdns::DiscoveredDevice;
use rustplay_rx::{CaptureOptions, MirrorTexts, ReceiverConfig, RxEvent, Scaling, VideoTexts, WindowOptions};
use rustplay_tx::MirrorOptions;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Direct2D::{ID2D1Bitmap1, ID2D1PathGeometry};
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, SC_MAXIMIZE, SC_MINIMIZE, SC_RESTORE, WM_CLOSE, WM_SYSCOMMAND,
};

use crate::backend::{Backend, CaptureAction, Command, Event, device_key};
use crate::gfx::{Color, Gfx, Rect};
use crate::i18n::{t, tf};
use crate::logbuf::LogEntry;
use crate::settings::{LATENCY_MS, RECV_SIZES, RESOLUTION_CAPS, Settings};
use crate::theme::Theme;

pub use logs::LogFilter;

const TITLE_H: f32 = 48.0;
const NAV_W: f32 = 300.0;
const PAD: f32 = 40.0;
const MAX_CONTENT: f32 = 1040.0;
const ROW_H: f32 = 68.0;
const GAP: f32 = 4.0;
const CAPTION_W: f32 = 46.0;
const CAPTION_H: f32 = 32.0;
const RECEIVER_PORT: u16 = 7000;
const PIN_LEN: usize = 4;
const PENDING_START_TRIES: u8 = 3;

mod glyph {
    pub const CAST: char = '\u{EC15}';
    pub const COLOR: char = '\u{E7E6}';
    pub const VIDEO: char = '\u{E714}';
    pub const PHOTO: char = '\u{E91B}';
    pub const RECEIVE: char = '\u{E975}';
    pub const INFO: char = '\u{E946}';
    pub const LOG: char = '\u{E9F9}';
    pub const PRIVACY: char = '\u{E72E}';
    pub const TV: char = '\u{E7F4}';
    pub const SPEAKER: char = '\u{E7F5}';
    pub const REFRESH: char = '\u{E72C}';
    pub const PC: char = '\u{E7F8}';
    pub const VOLUME: char = '\u{E767}';
    pub const RENAME: char = '\u{E8AC}';
    pub const PHONE: char = '\u{E8EA}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const CHEVRON_UP: char = '\u{E70E}';
    pub const SETTINGS: char = '\u{E713}';
    pub const QUALITY: char = '\u{E91B}';
    pub const SYNC: char = '\u{E895}';
    pub const POINTER: char = '\u{E8B0}';
    pub const WINDOW: char = '\u{E737}';
    pub const THEME: char = '\u{E790}';
    pub const LANGUAGE: char = '\u{E774}';
    pub const OPEN_EXTERNAL: char = '\u{E8A7}';
    pub const SEARCH: char = '\u{E721}';
    pub const RESET: char = '\u{E777}';
    pub const TRAY: char = '\u{E8A7}';
    pub const CLOSE: char = '\u{E711}';
    pub const ERROR: char = '\u{EB90}';
    pub const SUCCESS: char = '\u{EC61}';
    pub const STOP: char = '\u{E71A}';
    pub const WIFI: char = '\u{E701}';
    pub const POWER: char = '\u{E7E8}';
    pub const SCREEN: char = '\u{E7F4}';
    pub const COPY: char = '\u{E8C8}';
    pub const FOLDER: char = '\u{E838}';
    pub const DELETE: char = '\u{E74D}';
    pub const MINIMIZE: char = '\u{E921}';
    pub const MAXIMIZE: char = '\u{E922}';
    pub const RESTORE: char = '\u{E923}';
    pub const CHROME_CLOSE: char = '\u{E8BB}';
    pub const LOCK: char = '\u{E72E}';
    pub const PEOPLE: char = '\u{E716}';
    pub const STAR: char = '\u{E734}';
    pub const STAR_FILL: char = '\u{E735}';
    pub const EDIT: char = '\u{E70F}';
    pub const HISTORY: char = '\u{E81C}';
    pub const SCREENSHOT: char = '\u{E722}';
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Page {
    Send,
    Receive,
    History,
    Logs,
    About,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ComboKind {
    Language,
    Monitor,
    Source,
    Window,
    Fps,
    Quality,
    Resolution,
    AudioLatency,
    RecvSize,
    RecvFps,
    RecvScaling,
    RecvOutput,
    RecvKeep,
    RecvAccess,
    Theme,
    Accent,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum CaptureDir {
    Recordings,
    Screenshots,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LinkKind {
    GitHub,
    Coffee,
    PayPal,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Exp {
    SendQuality,
    SendAudio,
    RecvQuality,
    RecvWindow,
    RecvDisplay,
    Notifications,
    RecvSound,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Id {
    Nav(Page),
    NavIndicator,
    PageFade,
    Refresh,
    Device(usize),
    DeviceCard(usize),
    DeviceFav(usize),
    DeviceEdit(usize),
    // Star and pencil of a favourite that is not on the network now (index into the saved favourites).
    FavOffline(usize),
    FavOfflineEdit(usize),
    AliasBox,
    AliasOk,
    AliasCancel,
    HistoryClear,
    HistoryOpen(usize),
    HistoryReveal(usize),
    SaveHistory,
    HeroStop,
    Combo(ComboKind),
    ComboItem(ComboKind, usize),
    ComboDismiss,
    AudioToggle,
    RecvToggle,
    RecvAudio,
    RecvAppVideos,
    RecvAppPhotos,
    Autostart,
    NameBox,
    PinBox,
    PinConnect,
    PinCancel,
    ConfirmOk,
    ConfirmCancel,
    InfoClose,
    Modal,
    Minimize,
    Close,
    LogFilter(LogFilter),
    LogVerbose,
    LogCopy,
    LogFolder,
    LogClear,
    LogView,
    LogFollow,
    Toast,
    LinkArt,
    DeveloperLogs,
    HideIps,
    Expander(Exp),
    CursorToggle,
    AutoReconnect,
    RememberDevice,
    ConnectOnStart,
    RecvFullscreen,
    RecvOnTop,
    RecvBackdrop,
    RecvVsync,
    RecvHqScaling,
    RecvControls,
    RecvStats,
    RecvBorderless,
    RecvBlur,
    RecvLock,
    RecvMulti,
    RecvPin,
    DeviceAllow(usize),
    DeviceBlock(usize),
    DeviceForget(usize),
    CloseToTray,
    AutoDiscover,
    OpenData,
    ResetSettings,
    RecRecord,
    RecShot,
    RecReveal,
    RecordAudio,
    PickDir(CaptureDir),
    OpenDir(CaptureDir),
    Link(LinkKind),
    Notifications,
    NotifyConnections,
    NotifyErrors,
    UpdateAuto,
    UpdateCheck,
    UpdateOpen,
    UpdateInstall,
    UpdateInstallNow,
    UpdateCancel,
    UpdateSilent,
    SetupOpen,
    SetupFix(usize),
    SetupRecheck,
    SetupCopy,
    SetupClose,
    WelcomeOpen,
    WelcomeNext,
    WelcomeBack,
    WelcomeSkip,
    WelcomeCheck,
    WelcomeDot(usize),
    RemotePrevious,
    RemotePlayPause,
    RemoteNext,
    RecvDelay,
}

impl Id {
    fn text_input(self) -> bool {
        matches!(self, Id::NameBox | Id::PinBox | Id::AliasBox)
    }
}

// What the non-client hit test should report for a point in the window.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Client,
    Caption,
    Maximize,
}

struct InfoBar {
    title: String,
    message: String,
}

struct MonitorEntry {
    index: u32,
    name: Option<String>,
    width: u32,
    height: u32,
}

struct WindowEntry {
    hwnd: isize,
    title: String,
    process: String,
    icon: Option<ID2D1Bitmap1>,
}

struct DeviceEntry {
    device: DiscoveredDevice,
    key: String,
    name: String,
    model: String,
    address: String,
    speaker: bool,
}

impl DeviceEntry {
    // Model and address, without the address when IPs are hidden.
    fn detail(&self) -> String {
        let address = if crate::privacy::hide_ips() { "" } else { self.address.as_str() };
        [self.model.as_str(), address].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ·  ")
    }
}

#[derive(Clone, PartialEq)]
enum Mirror {
    Idle,
    Connecting { key: String },
    Reconnecting { key: String, attempt: u32 },
    Active { key: String, width: u32, height: u32 },
    Stopping { key: String },
}

impl Mirror {
    fn key(&self) -> Option<&str> {
        match self {
            Mirror::Idle => None,
            Mirror::Connecting { key }
            | Mirror::Reconnecting { key, .. }
            | Mirror::Active { key, .. }
            | Mirror::Stopping { key } => Some(key),
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReceiverState {
    Off,
    Starting,
    On { port: u16 },
    Stopping,
}

struct Client {
    peer: SocketAddr,
    name: String,
    status: Option<String>,
    // A device opens several connections (the session, its video, its event channel). It shows
    // once, by address, and goes away with the last of them.
    connections: u32,
}

// A receiver restart that applies changed options.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Restart {
    None,
    // Waiting for the connected devices to leave.
    Pending,
    // Stopped on purpose; start again when the stop completes.
    Running,
}

struct PinDialog {
    device: String,
    digits: String,
}

struct LogView {
    filter: LogFilter,
    entries: Vec<LogEntry>,
    heights: HashMap<u64, f32>,
    layout_width: f32,
    scroll: f32,
    max_scroll: f32,
    follow: bool,
    stale: bool,
    view: Rect,
}

pub struct App {
    hwnd: HWND,
    gfx: Gfx,
    theme: Theme,
    mica: bool,
    active: bool,
    maximized: bool,
    max_hover: bool,
    max_pressed: bool,
    visible: bool,
    logo: Option<ID2D1Bitmap1>,
    brand_icons: Vec<ID2D1PathGeometry>,
    backend: Option<Backend>,
    settings: Settings,
    monitors: Vec<MonitorEntry>,
    windows: Vec<WindowEntry>,
    // Handle of the window picked in this run; handles do not survive restarts, titles do.
    picked_window: Option<isize>,
    combo_scroll: usize,
    // Entry highlighted with the arrow keys in an open list, and how many entries fit.
    combo_hot: Option<usize>,
    combo_visible: usize,
    // Control with keyboard focus; its focus rectangle shows only after keyboard use.
    kb_focus: Option<Id>,
    focus_visible: bool,
    // Unclipped rects of the enabled controls drawn this frame, in drawing order.
    focusables: Vec<(Rect, Id)>,
    confirm: Option<keyboard::Confirm>,
    // The start-up animation already had its chance (it only plays once per run).
    intro_done: bool,
    // Design preview only: forces the state of the page illustrations.
    preview_link: Option<u8>,
    // Controls drawn now are inert (dimmed by a layer, no input); see `draw_receive`.
    inert: bool,
    ambient: bool,
    inert_layer: bool,
    last_device: String,
    // Discoveries left to wait for the last device to show up before connecting at startup.
    pending_start: Option<u8>,
    own_id: String,
    page: Page,
    devices: Vec<DeviceEntry>,
    discovering: bool,
    searched: bool,
    mirror: Mirror,
    cancelled: bool,
    receiver: ReceiverState,
    autostart: bool,
    clients: Vec<Client>,
    // The receiver is showing a mirrored screen (capture controls apply).
    mirroring: bool,
    recording_since: Option<Instant>,
    last_saved: Option<std::path::PathBuf>,
    restart: Restart,
    // Start of the entrance animation, the first time the window is shown.
    intro: Option<Instant>,
    attention: bool,
    running_name: String,
    // Whether the running receiver announced video playback (needs a restart to change).
    running_app_videos: bool,
    // Same for receiving photos and slideshows.
    running_app_photos: bool,
    pin: Option<PinDialog>,
    popups: Vec<access::PopupEntry>,
    next_popup_tag: u64,
    // PIN code the receiver is showing for a pairing in progress.
    rx_pin: Option<(SocketAddr, String)>,
    running_pin: bool,
    info: Option<InfoBar>,
    toast: Option<(String, Instant)>,
    combo: Option<ComboKind>,
    combo_anchor: Rect,
    logs: LogView,
    mouse: Option<(f32, f32)>,
    hover: Option<Id>,
    pressed: Option<Id>,
    focus: Option<Id>,
    regions: Vec<(Rect, Id)>,
    clip: Option<Rect>,
    anims: HashMap<(Id, u8), f32>,
    animating: bool,
    last_frame: Instant,
    started: Instant,
    caret_epoch: Instant,
    dt: f32,
    scroll: f32,
    scroll_max: f32,
    scroll_target: f32,
    expanded: std::collections::HashSet<Exp>,
    extras: extras::Extras,
    setup: Option<setup::Setup>,
    // Page of the welcome tour while it is open.
    welcome: Option<usize>,
    // When the tour page last changed and the page that is leaving (with its direction).
    welcome_at: Instant,
    welcome_prev: Option<(usize, Instant, f32)>,
    audio_ui: now_playing::AudioUi,
    alias_edit: Option<favorites::AliasEdit>,
    hist: history_page::HistoryUi,
}


fn hex_id(text: &str) -> String {
    text.chars().filter(char::is_ascii_hexdigit).collect::<String>().to_ascii_uppercase()
}

// Picks the open window that best matches a remembered title and process: same title and process,
// then same title, then same process.
fn find_window(open: &[(&str, &str)], title: &str, process: &str) -> Option<usize> {
    if title.is_empty() && process.is_empty() {
        return None;
    }
    let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    open.iter()
        .position(|(t, p)| same(t, title) && same(p, process))
        .or_else(|| open.iter().position(|(t, _)| !title.is_empty() && same(t, title)))
        .or_else(|| open.iter().position(|(_, p)| !process.is_empty() && same(p, process)))
}

fn friendly_model(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    if lower.starts_with("appletv") {
        String::from("Apple TV")
    } else if lower.starts_with("audioaccessory") {
        String::from("HomePod")
    } else if lower.starts_with("mac") {
        String::from("Mac")
    } else {
        model.to_owned()
    }
}

impl App {
    pub fn new(hwnd: HWND, gfx: Gfx, mica: bool, background: bool) -> Self {
        let logo = gfx.load_png(include_bytes!("../../assets/logo.png")).ok();
        let brand_icons = gfx
            .factory()
            .map(|factory| {
                [crate::brands::GITHUB, crate::brands::BUY_ME_A_COFFEE, crate::brands::PAYPAL]
                    .iter()
                    .filter_map(|data| crate::svg::geometry(&factory, data).ok())
                    .collect()
            })
            .unwrap_or_default();
        let settings = Settings::load();
        let names = crate::monitors::friendly_names();
        let monitors = rustplay_core::media::capture::list_monitors()
            .unwrap_or_default()
            .iter()
            .map(|m| MonitorEntry { index: m.index, name: names.get(&m.name).cloned(), width: m.width, height: m.height })
            .collect();
        let own_id = rustplay_core::identity::Identity::load_or_create()
            .map(|i| hex_id(&i.device_id_string()))
            .unwrap_or_default();
        let preview = crate::preview::active();
        let backend = if preview { Err(std::io::Error::other("vista previa")) } else { Backend::start(hwnd) };
        let backend = match backend {
            Ok(backend) => Some(backend),
            Err(e) => {
                tracing::error!("no se pudo iniciar el motor de red: {e}");
                None
            }
        };
        let now = Instant::now();
        let mut app = Self {
            hwnd,
            gfx,
            theme: Theme::resolve(settings.theme, settings.accent == 0),
            mica,
            active: true,
            maximized: false,
            max_hover: false,
            max_pressed: false,
            visible: !background,
            logo,
            brand_icons,
            backend,
            settings,
            monitors,
            windows: Vec::new(),
            picked_window: None,
            combo_scroll: 0,
            combo_hot: None,
            combo_visible: 0,
            kb_focus: None,
            focus_visible: false,
            focusables: Vec::new(),
            confirm: None,
            intro_done: false,
            preview_link: None,
            inert: false,
            ambient: false,
            inert_layer: false,
            last_device: String::new(),
            pending_start: None,
            own_id,
            page: Page::Send,
            devices: Vec::new(),
            discovering: false,
            searched: false,
            mirror: Mirror::Idle,
            cancelled: false,
            receiver: ReceiverState::Off,
            autostart: crate::autostart::is_enabled(),
            clients: Vec::new(),
            mirroring: false,
            recording_since: None,
            last_saved: None,
            restart: Restart::None,
            intro: (!background).then(Instant::now),
            attention: false,
            running_name: String::new(),
            running_app_videos: false,
            running_app_photos: false,
            pin: None,
            popups: Vec::new(),
            next_popup_tag: 0,
            rx_pin: None,
            running_pin: false,
            info: None,
            toast: None,
            combo: None,
            combo_anchor: Rect::default(),
            logs: LogView {
                filter: LogFilter::All,
                entries: Vec::new(),
                heights: HashMap::new(),
                layout_width: 0.0,
                scroll: 0.0,
                max_scroll: 0.0,
                follow: true,
                stale: true,
                view: Rect::default(),
            },
            mouse: None,
            hover: None,
            pressed: None,
            focus: None,
            regions: Vec::new(),
            clip: None,
            anims: HashMap::new(),
            animating: false,
            last_frame: now,
            started: now,
            caret_epoch: now,
            dt: 0.0,
            scroll: 0.0,
            scroll_max: 0.0,
            scroll_target: 0.0,
            expanded: std::collections::HashSet::new(),
            extras: extras::Extras::new(hwnd),
            setup: None,
            welcome: None,
            welcome_at: Instant::now(),
            welcome_prev: None,
            audio_ui: now_playing::AudioUi::default(),
            alias_edit: None,
            hist: history_page::HistoryUi::new(if preview { crate::history::Store::in_memory() } else { crate::history::Store::open() }),
        };
        app.audio_devices_refresh();
        if app.settings.monitor as usize >= app.monitors.len().max(1) {
            app.settings.monitor = 0;
        }
        app.last_device = app.settings.last_device.clone();
        if app.settings.connect_on_start && !app.last_device.is_empty() {
            app.pending_start = Some(PENDING_START_TRIES);
        }
        if preview {
            return app;
        }
        app.send(Command::Discover);
        crate::autostart::refresh();
        if app.settings.receiver_enabled || background {
            app.start_receiver();
        }
        app.update_tick();
        app
    }

    // ---------------------------------------------------------------- design preview

    pub fn preview_show(&mut self, page: Page, dark: bool, intro: bool) {
        self.preview_devices();
        self.preview_populate();
        self.page = page;
        self.combo = None;
        self.theme = crate::theme::Theme::resolve(if dark { 2 } else { 1 }, self.settings.accent == 0);
        crate::apply_dark_mode(self.hwnd, self.theme.dark);
        self.expanded = [Exp::SendQuality, Exp::SendAudio, Exp::RecvQuality, Exp::RecvWindow, Exp::RecvDisplay, Exp::Notifications, Exp::RecvSound]
            .into_iter()
            .collect();
        self.scroll = 0.0;
        self.scroll_target = 0.0;
        self.anims.clear();
        self.intro = if intro { Some(Instant::now()) } else { None };
    }

    // 0 off, 1 idle, 2 waiting, 3 connecting, 4 connected.
    pub fn preview_link(&mut self, link: Option<u8>) {
        self.preview_link = link;
    }

    pub fn preview_scroll(&mut self, fraction: f32) {
        self.scroll_target = self.scroll_max * fraction;
        self.scroll = self.scroll_target;
    }

    // ---------------------------------------------------------------- window state

    pub fn receiver_menu_state(&self) -> (bool, bool) {
        let on = matches!(self.receiver, ReceiverState::On { .. } | ReceiverState::Starting);
        (on, self.receiver == ReceiverState::Stopping)
    }

    pub fn tray_menu(&self) -> crate::tray::TrayMenu {
        let (receiver_on, receiver_busy) = self.receiver_menu_state();
        let current = self.mirror.key();
        crate::tray::TrayMenu {
            status: self.status_line().0,
            dark: self.theme.dark,
            receiver_on,
            receiver_busy,
            favorites: self.favorite_devices().into_iter().map(|(i, name)| (i, name, current == Some(self.devices[i].key.as_str()))).collect(),
            devices: self.devices.iter().map(|d| (self.shown_name(&d.key, &d.name), current == Some(d.key.as_str()))).collect(),
            mirroring: matches!(self.mirror, Mirror::Active { .. } | Mirror::Connecting { .. } | Mirror::Reconnecting { .. }),
            can_mirror: self.mirror == Mirror::Idle,
        }
    }

    pub fn tray_tip(&self) -> String {
        format!("RustPlay\n{}", self.status_line().0)
    }

    // Runs a command from the notification-area menu; returns true when the window should open.
    pub fn tray_command(&mut self, command: u32) -> bool {
        use crate::tray::{CMD_DEVICE, CMD_RECEIVER, CMD_SETTINGS, CMD_STOP_MIRROR};
        match command {
            CMD_RECEIVER => self.click(Id::RecvToggle),
            CMD_STOP_MIRROR => self.stop_mirror(),
            CMD_SETTINGS => {
                self.click(Id::Nav(Page::Settings));
                return true;
            }
            c if c >= CMD_DEVICE => self.mirror_device((c - CMD_DEVICE) as usize),
            _ => {}
        }
        false
    }

    // True once after something needs the user, such as a PIN while the window is hidden.
    pub fn take_attention(&mut self) -> bool {
        std::mem::take(&mut self.attention)
    }

    // With the receiver on, closing the window keeps RustPlay running in the notification area.
    pub fn keeps_running_hidden(&self) -> bool {
        self.settings.close_to_tray || self.receiver != ReceiverState::Off
    }


    pub fn refresh_theme(&mut self) {
        self.theme = Theme::resolve(self.settings.theme, self.settings.accent == 0);
        crate::apply_dark_mode(self.hwnd, self.theme.dark);
    }

    pub fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    pub fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
        // The start-up animation plays the first time the window shows, unless Windows has
        // animations turned off (Accessibility → Visual effects).
        if visible && self.intro.is_none() && !self.intro_done && animations_enabled() {
            self.intro = Some(Instant::now());
        }
        self.intro_done |= visible;
        if visible {
            self.maybe_open_onboarding();
        }
    }

    // Eased progress (0..1) of one part of the entrance animation, starting `delay` seconds in.
    fn intro(&mut self, delay: f32, duration: f32) -> f32 {
        let Some(start) = self.intro else { return 1.0 };
        let t = ((start.elapsed().as_secs_f32() - splash::SHELL_DELAY - delay) / duration).clamp(0.0, 1.0);
        if t < 1.0 {
            self.animating = true;
        }
        1.0 - (1.0 - t).powi(3)
    }

    pub fn set_maximized(&mut self, maximized: bool) {
        self.maximized = maximized;
    }

    pub fn set_max_hover(&mut self, hover: bool) -> bool {
        let changed = self.max_hover != hover;
        self.max_hover = hover;
        if !hover {
            self.max_pressed = false;
        }
        changed
    }

    pub fn press_max(&mut self) {
        self.max_pressed = true;
    }

    // Returns true when the click completed on the button.
    pub fn release_max(&mut self) -> bool {
        std::mem::take(&mut self.max_pressed) && self.max_hover
    }

    pub fn toggle_maximize(&self) {
        let command = if self.maximized { SC_RESTORE } else { SC_MAXIMIZE };
        unsafe {
            let _ = PostMessageW(Some(self.hwnd), WM_SYSCOMMAND, WPARAM(command as usize), LPARAM(0));
        }
    }

    fn caption_rects(&self) -> [Rect; 3] {
        let (w, _) = self.gfx.size_dip();
        let x = w - 3.0 * CAPTION_W;
        [
            Rect::new(x, 0.0, CAPTION_W, CAPTION_H),
            Rect::new(x + CAPTION_W, 0.0, CAPTION_W, CAPTION_H),
            Rect::new(x + 2.0 * CAPTION_W, 0.0, CAPTION_W, CAPTION_H),
        ]
    }

    pub fn nc_hit(&self, x: f32, y: f32) -> Hit {
        if self.pin.is_none() && self.caption_rects()[1].contains(x, y) {
            return Hit::Maximize;
        }
        if y < TITLE_H && self.hit(x, y).is_none() {
            return Hit::Caption;
        }
        Hit::Client
    }

    pub fn resize(&mut self, width: u32, height: u32, dpi: f32) {
        if let Err(e) = self.gfx.resize(width, height, dpi) {
            tracing::error!("no se pudo redimensionar la superficie: {e}");
        }
    }

    pub fn shutdown(&mut self) {
        self.history_shutdown();
        self.settings.save();
        if let Some(backend) = self.backend.take() {
            backend.shutdown();
        }
    }

    // Frames are only drawn while something moves and the window can be seen.
    pub fn frame_interval(&self) -> Option<u32> {
        if !self.visible {
            return None;
        }
        let busy = self.discovering
            || matches!(self.mirror, Mirror::Connecting { .. } | Mirror::Reconnecting { .. } | Mirror::Stopping { .. })
            || matches!(self.receiver, ReceiverState::Starting | ReceiverState::Stopping);
        if self.animating || busy {
            Some(16)
        } else if self.ambient && self.active {
            // Slow background motion (hero light, link comets) needs half the frames, and none
            // while RustPlay is not the active window, so it never competes with a mirror window.
            Some(33)
        } else if self.focus.is_some()
            || self.toast.is_some()
            || matches!(self.mirror, Mirror::Active { .. })
            || (self.page == Page::Receive && self.now_playing_ticks())
            || self.recording_since.is_some()
        {
            Some(265)
        } else {
            None
        }
    }

    fn send(&self, command: Command) {
        if let Some(backend) = &self.backend {
            backend.send(command);
        }
    }

    pub fn periodic_refresh(&self) {
        if self.settings.auto_discover && self.page == Page::Send && !self.discovering && self.pin.is_none() {
            self.send(Command::Discover);
        }
    }

    pub fn logs_changed(&mut self) -> bool {
        self.logs.stale = true;
        self.page == Page::Logs
    }

    // ---------------------------------------------------------------- backend events

    pub fn pump(&mut self) {
        while let Some(event) = self.backend.as_ref().and_then(Backend::poll) {
            self.on_event(event);
        }
        self.poll_updates();
        self.poll_install();
        self.poll_setup();
    }

    fn on_event(&mut self, event: Event) {
        self.notify_event(&event);
        self.history_event(&event);
        match event {
            Event::Discovering => self.discovering = true,
            Event::Devices(list) => {
                self.discovering = false;
                self.searched = true;
                let own = self.own_id.clone();
                let mut entries: Vec<DeviceEntry> = list
                    .into_iter()
                    .filter(|d| own.is_empty() || hex_id(&device_key(d)) != own)
                    .map(|device| {
                        let model = device.txt.get("model").cloned().unwrap_or_default();
                        let address = device
                            .addrs
                            .iter()
                            .find(|a| a.is_ipv4())
                            .or(device.addrs.first())
                            .map(ToString::to_string)
                            .unwrap_or_default();
                        DeviceEntry {
                            key: device_key(&device),
                            name: device.display_name().to_owned(),
                            speaker: model.starts_with("AudioAccessory") || model.contains("HomePod"),
                            model: friendly_model(&model),
                            address,
                            device,
                        }
                    })
                    .collect();
                entries.sort_by_key(|e| e.name.to_lowercase());
                self.devices = entries;
                self.refresh_favorites();
                self.try_pending_start();
            }
            Event::DiscoveryFailed(message) => {
                self.discovering = false;
                self.searched = true;
                self.show(t("error.discovery"), message);
            }
            Event::Connecting { key } => self.mirror = Mirror::Connecting { key },
            Event::Reconnecting { key, attempt } => {
                if !matches!(self.mirror, Mirror::Stopping { .. }) {
                    self.mirror = Mirror::Reconnecting { key, attempt };
                }
            }
            Event::PinRequired { name } => {
                self.pin = Some(PinDialog { device: name, digits: String::new() });
                self.attention = true;
                self.focus = Some(Id::PinBox);
                self.combo = None;
                self.caret_epoch = Instant::now();
                self.anims.remove(&(Id::Modal, 0));
            }
            Event::Mirroring { key, width, height, encoder } => {
                let name = self.device_name(&key).unwrap_or_default().to_owned();
                tracing::info!(?encoder, "duplicación iniciada en «{name}» ({width}x{height})");
                self.pin = None;
                self.focus = None;
                self.remember_device(&key);
                // A new transmission gets RustPlay out of the way; a reconnection after a drop
                // does not, so a window the user brought back stays.
                let fresh = matches!(self.mirror, Mirror::Connecting { .. });
                if !matches!(self.mirror, Mirror::Stopping { .. }) {
                    self.mirror = Mirror::Active { key, width, height };
                }
                if fresh && self.visible {
                    // SAFETY: ventana propia, desde su hilo de interfaz.
                    unsafe {
                        let _ = windows::Win32::UI::WindowsAndMessaging::ShowWindow(self.hwnd, windows::Win32::UI::WindowsAndMessaging::SW_MINIMIZE);
                    }
                }
            }
            Event::MirrorEnded { key, error } => {
                self.pin = None;
                if self.focus == Some(Id::PinBox) {
                    self.focus = None;
                }
                self.mirror = Mirror::Idle;
                let cancelled = std::mem::take(&mut self.cancelled);
                if let (Some(message), false) = (error, cancelled) {
                    let name = self.device_name(&key).unwrap_or(t("common.the_device")).to_owned();
                    self.show(&tf("error.mirror", &[("name", &name)]), message);
                }
            }
            Event::Receiver(event) => self.on_receiver(event),
            Event::ReceiverStopped { error } => {
                self.receiver = ReceiverState::Off;
                self.clients.clear();
                self.clear_now_playing();
                self.close_all_popups();
                self.mirroring = false;
                self.recording_since = None;
                if std::mem::replace(&mut self.restart, Restart::None) == Restart::Running && error.is_none() {
                    tracing::info!("aplicando los cambios del receptor");
                    self.start_receiver();
                    return;
                }
                if let Some(message) = error {
                    self.settings.receiver_enabled = false;
                    self.show(t("error.receiver_stopped"), message);
                }
            }
        }
    }

    // The custom name of a device if it has one, else its name on the network.
    fn device_name(&self, key: &str) -> Option<&str> {
        self.settings
            .alias(key)
            .or_else(|| self.devices.iter().find(|d| d.key == key).map(|d| d.name.as_str()))
            .or_else(|| self.settings.favorites.iter().find(|f| f.key == key).map(|f| f.name.as_str()).filter(|n| !n.is_empty()))
    }

    fn on_receiver(&mut self, event: RxEvent) {
        match event {
            RxEvent::Listening { port } => {
                self.receiver = ReceiverState::On { port };
            }
            RxEvent::Connected { peer, name } => {
                if let Some(client) = self.clients.iter_mut().find(|c| c.peer.ip() == peer.ip()) {
                    client.connections += 1;
                } else {
                    let name = name.unwrap_or_else(|| crate::privacy::redact(&peer.ip().to_string()).into_owned());
                    self.clients.push(Client { peer, name, status: None, connections: 1 });
                }
            }
            RxEvent::ClientName { peer, name } => {
                if let Some(client) = self.clients.iter_mut().find(|c| c.peer.ip() == peer.ip()) {
                    client.name = name;
                }
            }
            RxEvent::AudioStarted { peer, .. } => {
                self.set_status(peer, Some(t("receive.status.audio").to_owned()));
            }
            RxEvent::MirrorStarted { peer, width, height } => {
                self.mirroring = true;
                self.set_status(peer, Some(tf("receive.status.mirroring_size", &[("width", &width), ("height", &height)])));
            }
            RxEvent::MirrorStopped { peer } => {
                self.mirroring = false;
                self.recording_since = None;
                self.set_status(peer, None);
            }
            RxEvent::Volume { .. } => {}
            RxEvent::StreamStopped { peer } => {
                self.set_status(peer, None);
                self.clear_now_playing();
            }
            RxEvent::NowPlaying(now) => self.on_now_playing(*now),
            RxEvent::OpenApp => {
                self.attention = true;
                self.page = Page::Receive;
                self.combo = None;
            }
            RxEvent::VideoStarted { peer } => {
                self.set_status(peer, Some(t("receive.status.video").to_owned()));
            }
            RxEvent::VideoStopped => {
                let video = t("receive.status.video");
                for client in &mut self.clients {
                    if client.status.as_deref() == Some(video) {
                        client.status = None;
                    }
                }
            }
            RxEvent::PhotosStarted { peer } => {
                self.set_status(peer, Some(t("receive.status.photos").to_owned()));
            }
            RxEvent::PhotosStopped => {
                let photos = t("receive.status.photos");
                for client in &mut self.clients {
                    if client.status.as_deref() == Some(photos) {
                        client.status = None;
                    }
                }
            }
            RxEvent::MirrorSuspended { peer } => {
                self.set_status(peer, Some(t("receive.status.locked").to_owned()));
            }
            RxEvent::MirrorResumed { peer } => {
                self.set_status(peer, Some(t("receive.status.mirroring").to_owned()));
            }
            RxEvent::DeviceSeen { device_id, name, model, .. } => {
                self.note_device(&device_id, name.as_deref(), model.as_deref());
            }
            RxEvent::AccessRequest { id, peer, device_id, name } => {
                self.access_request(id, peer, &device_id, name.as_deref());
            }
            RxEvent::AccessResolved { id } => self.access_resolved(id),
            RxEvent::AccessDenied { .. } => {}
            RxEvent::PinShown { peer, pin } => self.pin_shown(peer, pin),
            RxEvent::PinFinished { peer, .. } => self.pin_finished(peer),
            RxEvent::RecordingState { active } => {
                self.recording_since = active.then(Instant::now);
            }
            RxEvent::RecordingSaved { path } => {
                self.last_saved = Some(path);
                self.toast(t("receive.capture.saved_video"));
            }
            RxEvent::ScreenshotSaved { path } => {
                self.last_saved = Some(path);
                self.toast(t("receive.capture.saved_shot"));
            }
            RxEvent::RecordingFailed { message } | RxEvent::ScreenshotFailed { message } => {
                self.show(t("receive.capture.failed"), message);
            }
            RxEvent::Disconnected { peer } => {
                if let Some(pos) = self.clients.iter().position(|c| c.peer.ip() == peer.ip()) {
                    self.clients[pos].connections = self.clients[pos].connections.saturating_sub(1);
                    if self.clients[pos].connections == 0 {
                        self.clients.remove(pos);
                    }
                }
                if self.clients.is_empty() {
                    self.clear_now_playing();
                    self.mirroring = false;
                    self.recording_since = None;
                }
                if self.clients.is_empty() && self.restart == Restart::Pending {
                    self.restart_receiver();
                }
            }
        }
    }

    fn set_status(&mut self, peer: std::net::SocketAddr, status: Option<String>) {
        if let Some(client) = self.clients.iter_mut().find(|c| c.peer.ip() == peer.ip()) {
            client.status = status;
        }
    }

    fn show(&mut self, title: &str, message: String) {
        self.info = Some(InfoBar { title: title.to_owned(), message });
    }

    fn toast(&mut self, text: &str) {
        self.toast = Some((text.to_owned(), Instant::now()));
        self.anims.remove(&(Id::Toast, 0));
    }

    // ---------------------------------------------------------------- actions

    fn start_receiver(&mut self) {
        let name = self.settings.receiver_name.trim().to_owned();
        if name.is_empty() {
            self.show(t("error.missing_name"), t("error.missing_name_body").to_owned());
            return;
        }
        self.settings.receiver_name = name.clone();
        self.running_name = name.clone();
        self.running_pin = self.settings.receiver_pin;
        self.running_app_videos = self.settings.receiver_app_videos;
        self.running_app_photos = self.settings.receiver_app_photos;
        self.settings.receiver_enabled = true;
        self.receiver = ReceiverState::Starting;
        self.send(Command::StartReceiver(self.receiver_config()));
    }

    fn receiver_config(&self) -> ReceiverConfig {
        let s = &self.settings;
        ReceiverConfig {
            name: self.running_name.clone(),
            port: RECEIVER_PORT,
            audio: s.receiver_audio,
            video: true,
            display_width: RECV_SIZES[s.receiver_size].0,
            display_height: RECV_SIZES[s.receiver_size].1,
            max_fps: s.receiver_fps,
            window: WindowOptions {
                fullscreen: s.receiver_fullscreen,
                always_on_top: s.receiver_on_top,
                lock_screen_backdrop: s.receiver_backdrop,
                scaling: match s.receiver_scaling {
                    1 => Scaling::Fill,
                    2 => Scaling::Stretch,
                    _ => Scaling::Fit,
                },
                vsync: s.receiver_vsync,
                high_quality_scaling: s.receiver_hq_scaling,
                controls: s.receiver_controls,
                open_app_button: true,
                stats: s.receiver_stats,
                borderless: s.receiver_borderless,
                blur_background: s.receiver_blur,
                lock_position: s.receiver_lock,
            },
            texts: MirrorTexts {
                lock_title: t("mirror.lock_title").to_owned(),
                lock_message: t("mirror.lock_message").to_owned(),
                unknown_device: t("mirror.unknown_device").to_owned(),
                fullscreen: t("mirror.controls.fullscreen").to_owned(),
                exit_fullscreen: t("mirror.controls.exit_fullscreen").to_owned(),
                open_app: t("mirror.controls.open_app").to_owned(),
                stop: t("mirror.controls.stop").to_owned(),
                record: t("mirror.controls.record").to_owned(),
                stop_recording: t("mirror.controls.stop_recording").to_owned(),
                screenshot: t("mirror.controls.screenshot").to_owned(),
                toast_fullscreen: t("mirror.toast.fullscreen").to_owned(),
                toast_recording: t("mirror.toast.recording").to_owned(),
                toast_screenshot: t("mirror.toast.screenshot").to_owned(),
                toast_mute: t("mirror.toast.mute").to_owned(),
                toast_blur: t("mirror.toast.blur").to_owned(),
                toast_cinema: t("mirror.toast.cinema").to_owned(),
                toast_lock: t("mirror.toast.lock").to_owned(),
                toast_stats: t("mirror.toast.stats").to_owned(),
                toast_topmost: t("mirror.toast.topmost").to_owned(),
                toast_on: t("mirror.toast.on").to_owned(),
                toast_off: t("mirror.toast.off").to_owned(),
                stats: t("mirror.stats").to_owned(),
                decoder_hardware: t("mirror.stats.hardware").to_owned(),
                decoder_software: t("mirror.stats.software").to_owned(),
                paused_title: t("mirror.paused_title").to_owned(),
                paused_message: t("mirror.paused_message").to_owned(),
            },
            audio_output: Some(s.receiver_output.clone()).filter(|id| !id.is_empty()),
            audio_delay_ms: s.receiver_audio_delay,
            keep_window: crate::settings::keep_window(s.receiver_keep),
            multiple_devices: s.receiver_multi,
            capture: CaptureOptions {
                recordings_dir: s.recordings_path(),
                screenshots_dir: s.screenshots_path(),
                record_audio: s.record_audio,
            },
            app_videos: s.receiver_app_videos,
            app_photos: s.receiver_app_photos,
            video_texts: VideoTexts {
                play: t("video.controls.play").to_owned(),
                pause: t("video.controls.pause").to_owned(),
                fullscreen: t("mirror.controls.fullscreen").to_owned(),
                exit_fullscreen: t("mirror.controls.exit_fullscreen").to_owned(),
                stop: t("video.controls.stop").to_owned(),
                volume: t("video.controls.volume").to_owned(),
                mute: t("video.controls.mute").to_owned(),
                unmute: t("video.controls.unmute").to_owned(),
                speed: t("video.controls.speed").to_owned(),
                quality: t("video.controls.quality").to_owned(),
                quality_auto: t("video.controls.quality_auto").to_owned(),
                subtitles: t("video.controls.subtitles").to_owned(),
                subtitles_off: t("video.controls.subtitles_off").to_owned(),
                live: t("video.controls.live").to_owned(),
            },
            video_languages: vec![crate::i18n::code().to_owned()],
            loopback_only: false,
            pin: self.pin_config(),
            access: self.access_config(),
        }
    }

    // Sends the current options to a running receiver, which applies them at once.
    fn update_receiver(&mut self) {
        if matches!(self.receiver, ReceiverState::On { .. } | ReceiverState::Starting) {
            self.send(Command::UpdateReceiver(self.receiver_config()));
        }
    }

    fn capture_dir(&self, kind: CaptureDir) -> std::path::PathBuf {
        match kind {
            CaptureDir::Recordings => self.settings.recordings_path(),
            CaptureDir::Screenshots => self.settings.screenshots_path(),
        }
    }

    fn receiver_editable(&self) -> bool {
        matches!(self.receiver, ReceiverState::Off | ReceiverState::On { .. })
    }

    // Saves receiver options and applies them to a running receiver at once. Only a new name is
    // announced on the network, so it restarts the receiver (now, or once no device is connected).
    fn receiver_changed(&mut self) {
        self.settings.save();
        if !matches!(self.receiver, ReceiverState::On { .. }) {
            return;
        }
        if self.settings.receiver_name.trim() == self.running_name
            && self.settings.receiver_pin == self.running_pin
            && self.settings.receiver_app_videos == self.running_app_videos
            && self.settings.receiver_app_photos == self.running_app_photos
        {
            self.update_receiver();
            return;
        }
        if self.clients.is_empty() {
            self.restart_receiver();
        } else {
            self.restart = Restart::Pending;
        }
    }

    fn restart_receiver(&mut self) {
        self.restart = Restart::Running;
        self.receiver = ReceiverState::Stopping;
        self.send(Command::StopReceiver);
    }

    fn commit_name(&mut self) {
        self.focus = None;
        if self.settings.receiver_name.trim().is_empty() {
            self.settings.receiver_name = self.running_name.clone();
        }
        if self.settings.receiver_name.trim() != self.running_name {
            self.receiver_changed();
        } else {
            self.settings.save();
        }
    }

    fn mirror_device(&mut self, index: usize) {
        let Some(entry) = self.devices.get(index) else { return };
        let (key, device) = (entry.key.clone(), entry.device.clone());
        match &self.mirror {
            Mirror::Idle => {
                let window = if self.settings.share_window {
                    match self.resolve_window() {
                        Some(hwnd) => Some(hwnd),
                        None => {
                            self.show(t("error.window_gone"), t("error.window_gone_body").to_owned());
                            return;
                        }
                    }
                } else {
                    None
                };
                let options = MirrorOptions {
                    monitor: self.settings.monitor,
                    window,
                    fps: self.settings.fps,
                    bitrate: self.settings.bitrate(),
                    audio: self.settings.audio,
                    volume_db: None,
                    max_height: RESOLUTION_CAPS[self.settings.resolution],
                    cursor: self.settings.cursor,
                    audio_latency_ms: LATENCY_MS[self.settings.audio_latency],
                };
                self.info = None;
                self.mirror = Mirror::Connecting { key };
                self.send(Command::Mirror { device: Box::new(device), options, reconnect: self.settings.auto_reconnect });
            }
            Mirror::Connecting { key: current } | Mirror::Reconnecting { key: current, .. } if *current == key => {
                self.cancelled = true;
                self.mirror = Mirror::Stopping { key: current.clone() };
                self.send(Command::StopMirror);
            }
            Mirror::Active { key: current, .. } if *current == key => self.stop_mirror(),
            _ => {}
        }
    }

    fn stop_mirror(&mut self) {
        if let Mirror::Active { key, .. } | Mirror::Connecting { key } | Mirror::Reconnecting { key, .. } = &self.mirror {
            if matches!(self.mirror, Mirror::Connecting { .. } | Mirror::Reconnecting { .. }) {
                self.cancelled = true;
            }
            self.mirror = Mirror::Stopping { key: key.clone() };
            self.send(Command::StopMirror);
        }
    }

    fn remember_device(&mut self, key: &str) {
        self.last_device = key.to_owned();
        let stored = if self.settings.remember_device { key.to_owned() } else { String::new() };
        if self.settings.last_device != stored {
            self.settings.last_device = stored;
            self.settings.save();
        }
    }

    fn start_known_device(&mut self) {
        if let Some(index) = self.devices.iter().position(|d| d.key == self.last_device) {
            self.mirror_device(index);
        }
    }

    fn try_pending_start(&mut self) {
        let Some(left) = self.pending_start else { return };
        if self.mirror != Mirror::Idle || self.last_device.is_empty() {
            self.pending_start = None;
        } else if self.devices.iter().any(|d| d.key == self.last_device) {
            self.pending_start = None;
            self.start_known_device();
        } else if left > 1 {
            self.pending_start = Some(left - 1);
            self.send(Command::Discover);
        } else {
            self.pending_start = None;
            self.toast(t("send.last_device_missing"));
        }
    }

    // ---------------------------------------------------------------- window picker

    fn refresh_windows(&mut self) {
        let gfx = &self.gfx;
        self.windows = rustplay_core::media::capture::list_windows()
            .into_iter()
            .map(|w| WindowEntry {
                icon: crate::window_icons::icon_of(w.hwnd).and_then(|i| gfx.bitmap_from_bgra(i.width, i.height, &i.pixels).ok()),
                hwnd: w.hwnd,
                title: w.title,
                process: w.process,
            })
            .collect();
    }

    // The window to share: the one picked in this run if it is still open, else the one that matches
    // the remembered title and process.
    fn resolve_window(&self) -> Option<isize> {
        let open = rustplay_core::media::capture::list_windows();
        if let Some(hwnd) = self.picked_window.filter(|h| open.iter().any(|w| w.hwnd == *h)) {
            return Some(hwnd);
        }
        let candidates: Vec<(&str, &str)> = open.iter().map(|w| (w.title.as_str(), w.process.as_str())).collect();
        find_window(&candidates, &self.settings.window_title, &self.settings.window_process).map(|i| open[i].hwnd)
    }

    fn selected_window_index(&self) -> Option<usize> {
        if let Some(i) = self.picked_window.and_then(|h| self.windows.iter().position(|w| w.hwnd == h)) {
            return Some(i);
        }
        let candidates: Vec<(&str, &str)> = self.windows.iter().map(|w| (w.title.as_str(), w.process.as_str())).collect();
        find_window(&candidates, &self.settings.window_title, &self.settings.window_process)
    }

    fn click(&mut self, id: Id) {
        if self.audio_click(id) {
            return;
        }
        if self.favorites_click(id) || self.history_click(id) {
            return;
        }
        match id {
            Id::Nav(page) => {
                if self.page != page {
                    self.page = page;
                    self.scroll = 0.0;
                    self.scroll_target = 0.0;
                    self.combo = None;
                    self.anims.insert((Id::PageFade, 0), 0.0);
                    if page == Page::Logs {
                        self.logs.stale = true;
                        self.logs.follow = true;
                    }
                }
            }
            Id::Refresh => self.send(Command::Discover),
            Id::Device(index) | Id::DeviceCard(index) => {
                if id == Id::Device(index) || self.mirror == Mirror::Idle {
                    self.mirror_device(index);
                }
            }
            Id::HeroStop => self.stop_mirror(),
            Id::Combo(kind) => {
                self.combo_scroll = 0;
                self.combo_hot = None;
                if self.combo == Some(kind) {
                    self.combo = None;
                } else {
                    if kind == ComboKind::Window {
                        self.refresh_windows();
                    }
                    if kind == ComboKind::RecvOutput {
                        self.audio_devices_refresh();
                    }
                    self.combo = Some(kind);
                }
            }
            Id::ComboItem(kind, index) => {
                match kind {
                    ComboKind::Monitor => self.settings.monitor = index as u32,
                    ComboKind::Source => {
                        self.settings.share_window = index == 1;
                        if self.settings.share_window {
                            self.refresh_windows();
                        }
                    }
                    ComboKind::Window => {
                        let Some(entry) = self.windows.get(index) else {
                            self.combo = None;
                            return;
                        };
                        self.picked_window = Some(entry.hwnd);
                        self.settings.window_title = entry.title.clone();
                        self.settings.window_process = entry.process.clone();
                    }
                    ComboKind::Fps => self.settings.fps = if index == 1 { 60 } else { 30 },
                    ComboKind::Quality => self.settings.quality = index,
                    ComboKind::Resolution => self.settings.resolution = index,
                    ComboKind::AudioLatency => self.settings.audio_latency = index,
                    ComboKind::RecvSize | ComboKind::RecvFps => {
                        if kind == ComboKind::RecvSize {
                            self.settings.receiver_size = index;
                        } else {
                            self.settings.receiver_fps = if index == 0 { 30 } else { 60 };
                        }
                        self.combo = None;
                        self.receiver_changed();
                        return;
                    }
                    ComboKind::RecvOutput => {
                        self.select_output(index);
                        self.combo = None;
                        self.receiver_changed();
                        return;
                    }
                    ComboKind::RecvKeep => {
                        self.settings.receiver_keep = index;
                        self.combo = None;
                        self.receiver_changed();
                        return;
                    }
                    ComboKind::RecvAccess => {
                        self.settings.access_policy = index;
                        self.combo = None;
                        self.receiver_changed();
                        return;
                    }
                    ComboKind::RecvScaling => {
                        self.settings.receiver_scaling = index;
                        self.combo = None;
                        self.receiver_changed();
                        return;
                    }
                    ComboKind::Language => {
                        self.settings.language = index;
                        crate::i18n::apply(index);
                        self.update_receiver();
                    }
                    ComboKind::Theme => {
                        self.settings.theme = index;
                        self.refresh_theme();
                    }
                    ComboKind::Accent => {
                        self.settings.accent = index;
                        self.refresh_theme();
                    }
                }
                self.combo = None;
                self.settings.save();
            }
            Id::ComboDismiss => self.combo = None,
            Id::Expander(kind) => {
                if !self.expanded.remove(&kind) {
                    self.expanded.insert(kind);
                }
            }
            Id::CursorToggle => {
                self.settings.cursor = !self.settings.cursor;
                self.settings.save();
            }
            Id::AutoReconnect => {
                self.settings.auto_reconnect = !self.settings.auto_reconnect;
                self.settings.save();
            }
            Id::RememberDevice => {
                self.settings.remember_device = !self.settings.remember_device;
                if self.settings.remember_device {
                    self.settings.last_device = self.last_device.clone();
                } else {
                    self.settings.last_device.clear();
                    self.settings.connect_on_start = false;
                }
                self.settings.save();
            }
            Id::ConnectOnStart if self.settings.remember_device => {
                self.settings.connect_on_start = !self.settings.connect_on_start;
                self.settings.save();
            }
            Id::RecvFullscreen if self.receiver_editable() => {
                self.settings.receiver_fullscreen = !self.settings.receiver_fullscreen;
                self.receiver_changed();
            }
            Id::RecvOnTop if self.receiver_editable() => {
                self.settings.receiver_on_top = !self.settings.receiver_on_top;
                self.receiver_changed();
            }
            Id::RecvBackdrop if self.receiver_editable() => {
                self.settings.receiver_backdrop = !self.settings.receiver_backdrop;
                self.receiver_changed();
            }
            Id::RecvVsync if self.receiver_editable() => {
                self.settings.receiver_vsync = !self.settings.receiver_vsync;
                self.receiver_changed();
            }
            Id::RecvHqScaling if self.receiver_editable() => {
                self.settings.receiver_hq_scaling = !self.settings.receiver_hq_scaling;
                self.receiver_changed();
            }
            Id::RecvControls if self.receiver_editable() => {
                self.settings.receiver_controls = !self.settings.receiver_controls;
                self.receiver_changed();
            }
            Id::RecvMulti if self.receiver_editable() => {
                self.settings.receiver_multi = !self.settings.receiver_multi;
                self.receiver_changed();
            }
            Id::RecvStats if self.receiver_editable() => {
                self.settings.receiver_stats = !self.settings.receiver_stats;
                self.receiver_changed();
            }
            Id::RecvBorderless if self.receiver_editable() => {
                self.settings.receiver_borderless = !self.settings.receiver_borderless;
                self.receiver_changed();
            }
            Id::RecvBlur if self.receiver_editable() => {
                self.settings.receiver_blur = !self.settings.receiver_blur;
                self.receiver_changed();
            }
            Id::RecvLock if self.receiver_editable() => {
                self.settings.receiver_lock = !self.settings.receiver_lock;
                self.receiver_changed();
            }
            Id::RecvPin if self.receiver_editable() => {
                self.settings.receiver_pin = !self.settings.receiver_pin;
                self.receiver_changed();
            }
            Id::DeviceAllow(i) => self.device_action(i, Some(crate::settings::DeviceState::Allowed)),
            Id::DeviceBlock(i) => self.device_action(i, Some(crate::settings::DeviceState::Blocked)),
            Id::DeviceForget(i) => self.ask_confirm(keyboard::ConfirmAction::ForgetDevice(i)),
            Id::CloseToTray => {
                self.settings.close_to_tray = !self.settings.close_to_tray;
                self.settings.save();
            }
            Id::DeveloperLogs => {
                self.settings.developer_logs = !self.settings.developer_logs;
                if !self.settings.developer_logs && self.page == Page::Logs {
                    self.page = Page::Settings;
                }
                self.settings.save();
            }
            Id::HideIps => {
                self.settings.hide_ips = !self.settings.hide_ips;
                crate::privacy::set_hide_ips(self.settings.hide_ips);
                self.settings.save();
            }
            Id::AutoDiscover => {
                self.settings.auto_discover = !self.settings.auto_discover;
                self.settings.save();
            }
            Id::OpenData => logs::open_folder(),
            Id::RecRecord if self.mirroring => self.send(Command::Capture(CaptureAction::ToggleRecording)),
            Id::RecShot if self.mirroring => self.send(Command::Capture(CaptureAction::Screenshot)),
            Id::RecReveal => {
                if let Some(path) = &self.last_saved {
                    crate::folder::reveal(path);
                }
            }
            Id::RecordAudio => {
                self.settings.record_audio = !self.settings.record_audio;
                self.receiver_changed();
            }
            Id::OpenDir(kind) => crate::folder::open(&self.capture_dir(kind)),
            Id::PickDir(kind) => {
                let current = self.capture_dir(kind);
                if let Some(path) = crate::folder::pick(self.hwnd, t("receive.capture.pick_title"), &current) {
                    let text = path.display().to_string();
                    match kind {
                        CaptureDir::Recordings => self.settings.recordings_dir = text,
                        CaptureDir::Screenshots => self.settings.screenshots_dir = text,
                    }
                    self.receiver_changed();
                }
            }
            Id::Link(kind) => crate::links::open(match kind {
                LinkKind::GitHub => crate::links::GITHUB,
                LinkKind::Coffee => crate::links::KOFI,
                LinkKind::PayPal => crate::links::PAYPAL,
            }),
            Id::ResetSettings => self.ask_confirm(keyboard::ConfirmAction::ResetSettings),
            Id::ConfirmOk => self.confirm_answer(true),
            Id::ConfirmCancel => self.confirm_answer(false),
            Id::AudioToggle => {
                self.settings.audio = !self.settings.audio;
                self.settings.save();
            }
            Id::RecvToggle => match self.receiver {
                ReceiverState::Off => {
                    self.focus = None;
                    self.start_receiver();
                    self.settings.save();
                }
                ReceiverState::On { .. } | ReceiverState::Starting => {
                    self.restart = Restart::None;
                    self.settings.receiver_enabled = false;
                    self.receiver = ReceiverState::Stopping;
                    self.send(Command::StopReceiver);
                    self.settings.save();
                }
                ReceiverState::Stopping => {}
            },
            Id::RecvAudio if self.receiver_editable() => {
                self.settings.receiver_audio = !self.settings.receiver_audio;
                self.receiver_changed();
            }
            Id::RecvAppVideos if self.receiver_editable() => {
                self.settings.receiver_app_videos = !self.settings.receiver_app_videos;
                self.receiver_changed();
            }
            Id::RecvAppPhotos if self.receiver_editable() => {
                self.settings.receiver_app_photos = !self.settings.receiver_app_photos;
                self.receiver_changed();
            }
            Id::Autostart => {
                let wanted = !self.autostart;
                if crate::autostart::set(wanted) {
                    self.autostart = wanted;
                } else {
                    self.show(
                        t("error.autostart"),
                        t("error.autostart_body").to_owned(),
                    );
                }
            }
            Id::Notifications | Id::NotifyConnections | Id::NotifyErrors | Id::UpdateAuto
            | Id::UpdateCheck
            | Id::UpdateOpen
            | Id::UpdateInstall
            | Id::UpdateInstallNow
            | Id::UpdateCancel
            | Id::UpdateSilent => {
                self.extras_click(id);
            }
            Id::SetupOpen | Id::SetupFix(_) | Id::SetupRecheck | Id::SetupCopy | Id::SetupClose => self.setup_click(id),
            Id::WelcomeOpen | Id::WelcomeNext | Id::WelcomeBack | Id::WelcomeSkip | Id::WelcomeCheck | Id::WelcomeDot(_) => {
                self.welcome_click(id);
            }
            Id::PinConnect => self.submit_pin(),
            Id::PinCancel => self.cancel_pin(),
            Id::InfoClose => self.info = None,
            Id::Minimize => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_SYSCOMMAND, WPARAM(SC_MINIMIZE as usize), LPARAM(0));
            },
            Id::Close => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            Id::LogFilter(filter) => {
                self.logs.filter = filter;
                self.logs.stale = true;
                self.logs.follow = true;
            }
            Id::LogVerbose => {
                let buffer = crate::logbuf::buffer();
                buffer.set_verbose(!buffer.verbose());
            }
            Id::LogCopy => self.copy_logs(),
            Id::LogFolder => logs::open_folder(),
            Id::LogFollow => self.logs.follow = true,
            Id::LogClear => {
                crate::logbuf::buffer().clear();
                self.logs.stale = true;
            }
            _ => {}
        }
    }

    fn submit_pin(&mut self) {
        let Some(dialog) = &self.pin else { return };
        if dialog.digits.len() < PIN_LEN {
            return;
        }
        let pin = dialog.digits.clone();
        self.pin = None;
        self.focus = None;
        self.send(Command::Pin(Some(pin)));
    }

    fn cancel_pin(&mut self) {
        if self.pin.take().is_some() {
            self.focus = None;
            self.cancelled = true;
            self.send(Command::Pin(None));
        }
    }

    fn reset_settings(&mut self) {
        let name = self.settings.receiver_name.clone();
        let enabled = self.settings.receiver_enabled;
        let notified = std::mem::take(&mut self.settings.update_notified);
        let onboarding_done = self.settings.onboarding_done;
        let known_devices = std::mem::take(&mut self.settings.known_devices);
        let favorites = std::mem::take(&mut self.settings.favorites);
        let aliases = std::mem::take(&mut self.settings.aliases);
        self.settings = Settings {
            receiver_name: name,
            receiver_enabled: enabled,
            update_notified: notified,
            onboarding_done,
            known_devices,
            favorites,
            aliases,
            ..Settings::default()
        };
        self.settings.save();
        self.refresh_theme();
        crate::i18n::apply(self.settings.language);
        self.toast(t("settings.reset_done"));
    }

    // ---------------------------------------------------------------- input

    fn hit(&self, x: f32, y: f32) -> Option<Id> {
        self.regions.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, id)| *id)
    }

    pub fn mouse_move(&mut self, x: f32, y: f32) -> bool {
        self.mouse = Some((x, y));
        if self.pressed == Some(Id::RecvDelay) && self.drag_delay(x) {
            return true;
        }
        let hover = self.hit(x, y);
        let changed = hover != self.hover || self.max_hover;
        self.hover = hover;
        self.max_hover = false;
        changed
    }

    pub fn mouse_leave(&mut self) {
        self.mouse = None;
        self.hover = None;
    }

    pub fn mouse_down(&mut self, x: f32, y: f32) {
        if self.skip_splash() {
            return;
        }
        self.mouse_move(x, y);
        self.pressed = self.hover;
        // The pointer hides the focus rectangle; the next Tab continues from what was clicked.
        self.focus_visible = false;
        if let Some(id) = self.hover {
            self.kb_focus = Some(id);
        }
        if self.pressed == Some(Id::RecvDelay) {
            self.drag_delay(x);
        }
        if self.pin.is_none() {
            let was_editing = self.focus == Some(Id::NameBox);
            self.focus = self.hover.filter(|id| *id == Id::NameBox && self.receiver_editable());
            if was_editing && self.focus.is_none() {
                self.commit_name();
            }
            // A click outside the custom-name editor saves it, as in a flyout.
            if self.alias_edit.is_some() {
                match self.hover {
                    Some(Id::AliasBox) => self.focus = Some(Id::AliasBox),
                    Some(Id::AliasOk | Id::AliasCancel) => {}
                    _ => self.commit_alias(),
                }
            }
        }
        if self.focus.is_some() {
            self.caret_epoch = Instant::now();
        }
    }

    pub fn mouse_up(&mut self, x: f32, y: f32) {
        self.mouse_move(x, y);
        let pressed = self.pressed.take();
        if pressed == Some(Id::RecvDelay) {
            self.click(Id::RecvDelay);
            return;
        }
        if let Some(id) = pressed.filter(|id| Some(*id) == self.hover) {
            self.click(id);
        }
    }

    pub fn over_text_input(&self) -> bool {
        self.hover.is_some_and(Id::text_input)
    }

    pub fn wheel(&mut self, delta: f32) {
        if self.confirm.is_none() && self.pin.is_none() && self.setup_wheel(delta) {
            return;
        }
        if self.pin.is_some() || self.confirm.is_some() || self.welcome_open() {
            return;
        }
        if self.combo == Some(ComboKind::Window) {
            self.combo_scroll = if delta > 0.0 { self.combo_scroll.saturating_sub(1) } else { self.combo_scroll + 1 };
            return;
        }
        self.combo = None;
        // The wheel moves the lip-sync slider by single 10 ms steps.
        if self.hover == Some(Id::RecvDelay) {
            self.nudge_delay(if delta > 0.0 { 1 } else { -1 });
            return;
        }
        if self.page == Page::Logs {
            let logs = &mut self.logs;
            logs.scroll = (logs.scroll - delta * 60.0).clamp(0.0, logs.max_scroll);
            logs.follow = logs.scroll >= logs.max_scroll - 1.0;
            return;
        }
        self.scroll_target = (self.scroll_target - delta * 96.0).clamp(0.0, self.scroll_max);
    }

    pub fn key_char(&mut self, c: char) {
        self.caret_epoch = Instant::now();
        if let Some(dialog) = self.pin.as_mut() {
            match c {
                '0'..='9' if dialog.digits.len() < PIN_LEN => dialog.digits.push(c),
                '\u{8}' => {
                    dialog.digits.pop();
                }
                '\r' => self.submit_pin(),
                '\u{1b}' => self.cancel_pin(),
                _ => {}
            }
            return;
        }
        if self.focus == Some(Id::AliasBox) {
            self.alias_char(c);
        } else if self.focus == Some(Id::NameBox) {
            let name = &mut self.settings.receiver_name;
            match c {
                '\u{8}' => {
                    name.pop();
                }
                '\r' | '\u{1b}' | '\t' => self.commit_name(),
                c if !c.is_control() && name.chars().count() < 48 => name.push(c),
                _ => {}
            }
        } else if c == '\u{1b}' {
            self.combo = None;
            self.info = None;
            self.focus_visible = false;
        }
    }

    pub fn paste(&mut self, text: &str) {
        if let Some(dialog) = self.pin.as_mut() {
            for c in text.chars().filter(char::is_ascii_digit) {
                if dialog.digits.len() < PIN_LEN {
                    dialog.digits.push(c);
                }
            }
        } else if self.focus == Some(Id::AliasBox) {
            self.alias_paste(text);
        } else if self.focus == Some(Id::NameBox) {
            for c in text.chars().filter(|c| !c.is_control()) {
                if self.settings.receiver_name.chars().count() < 48 {
                    self.settings.receiver_name.push(c);
                }
            }
        }
    }

    pub fn focus_lost(&mut self) {
        if self.focus == Some(Id::NameBox) {
            self.commit_name();
        }
        if self.alias_edit.is_some() {
            self.commit_alias();
        }
        self.pressed = None;
    }

    // ---------------------------------------------------------------- frame

    pub fn render(&mut self) {
        let now = Instant::now();
        self.dt = now.duration_since(self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        self.animating = false;
        self.ambient = false;
        self.regions.clear();
        self.focusables.clear();
        let background = if self.mica && !self.theme.high_contrast { Color::TRANSPARENT } else { self.theme.solid_background };
        self.gfx.begin(background);
        let (w, h) = self.gfx.size_dip();
        self.draw_title_bar(w);
        self.draw_nav(h);
        let layer = Rect::new(NAV_W, TITLE_H, w - NAV_W + 16.0, h - TITLE_H + 16.0);
        self.gfx.fill(layer, 8.0, self.theme.layer);
        self.gfx.stroke(layer, 8.0, 1.0, self.theme.layer_stroke);
        let fade = self.anim((Id::PageFade, 0), 1.0, 14.0);
        let entrance = self.intro(0.18, 0.55);
        let eased = (1.0 - (1.0 - fade).powi(3)) * entrance;
        self.gfx.push_layer(eased, 18.0 * (1.0 - eased) + 14.0 * (1.0 - entrance));
        self.draw_page(w, h);
        self.gfx.pop_layer();
        if self.combo.is_some() {
            self.draw_combo_popup(w, h);
        }
        self.draw_toast(w, h);
        self.draw_splash(w, h);
        if self.setup.is_some() {
            self.draw_setup(w, h);
        }
        self.maybe_open_onboarding();
        if self.welcome.is_some() {
            self.draw_welcome(w, h);
        }
        if self.pin.is_some() {
            self.draw_pin(w, h);
        }
        if self.confirm.is_some() {
            self.draw_confirm(w, h);
        }
        self.draw_focus();
        if let Err(e) = self.gfx.end() {
            tracing::error!("error al presentar el fotograma: {e}");
        }
        if let Some((mx, my)) = self.mouse {
            self.hover = self.hit(mx, my);
        }
    }
}

// False when the user turned off "Animation effects" in Windows.
fn animations_enabled() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW};
    let mut enabled = windows::core::BOOL(1);
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL into `enabled`.
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(std::ptr::from_mut(&mut enabled).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    ok.is_err() || enabled.as_bool()
}

#[cfg(test)]
mod tests {
    use super::find_window;

    #[test]
    fn remembered_window_matches_title_and_process_first() {
        let open = [("Notas", "notepad.exe"), ("Notas", "code.exe"), ("Otro", "code.exe")];
        assert_eq!(find_window(&open, "notas", "CODE.EXE"), Some(1));
    }

    #[test]
    fn remembered_window_falls_back_to_title_then_process() {
        let open = [("Vídeo", "player.exe"), ("Otra", "browser.exe")];
        assert_eq!(find_window(&open, "Otra", "chrome.exe"), Some(1));
        assert_eq!(find_window(&open, "Pestaña nueva", "player.exe"), Some(0));
        assert_eq!(find_window(&open, "Nada", "nada.exe"), None);
        assert_eq!(find_window(&open, "", ""), None);
    }
}

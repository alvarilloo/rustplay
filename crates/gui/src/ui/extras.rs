// Notifications and the update check, hooked into App from ui/mod.rs.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rustplay_rx::RxEvent;

use super::widgets::Style;
use super::{App, Exp, GAP, Id, Mirror, ROW_H};
use crate::gfx::{Align, Font, Rect};
use crate::backend::Event;
use crate::i18n::{t, tf};
use crate::notify::Notifier;
use crate::update::{Outcome, Release};
use crate::updater::{self, Download, InstallerKind};

const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);
const RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, PartialEq, Eq)]
pub enum UpdateState {
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed,
}

// Progress of «Descargar e instalar».
pub enum Install {
    Idle,
    Downloading(Download, InstallerKind),
    // Downloaded and verified; waiting for the user to confirm.
    Ready(std::path::PathBuf, InstallerKind),
    // Language key of what went wrong.
    Failed(&'static str),
}

pub struct Extras {
    pub install: Install,
    notifier: Notifier,
    // Receiver clients already announced, so each connection is announced once.
    announced: HashSet<SocketAddr>,
    pub update: UpdateState,
    pending: Option<mpsc::Receiver<Outcome>>,
    next_check: Option<Instant>,
}

impl Extras {
    pub fn new(hwnd: windows::Win32::Foundation::HWND) -> Self {
        Self { install: Install::Idle, notifier: Notifier::new(hwnd), announced: HashSet::new(), update: UpdateState::Idle, pending: None, next_check: None }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Connection,
    Error,
    Update,
}

impl App {
    fn notify(&mut self, kind: Kind, title: &str, body: &str) {
        let s = &self.settings;
        let wanted = s.notifications
            && match kind {
                Kind::Connection => s.notify_connections,
                Kind::Error => s.notify_errors,
                Kind::Update => true,
            };
        if wanted {
            self.extras.notifier.show(title, body);
        }
    }

    fn client_name(&self, peer: SocketAddr) -> String {
        self.clients
            .iter()
            .find(|c| c.peer == peer)
            .map_or_else(|| t("mirror.unknown_device").to_owned(), |c| c.name.clone())
    }

    // Called with each backend event before App applies it, so the previous state is still there.
    pub(super) fn notify_event(&mut self, event: &Event) {
        match event {
            Event::Mirroring { key, .. } => {
                let name = self.device_name(key).unwrap_or(t("common.the_tv")).to_owned();
                self.notify(Kind::Connection, &tf("notify.send.started", &[("name", &name)]), t("notify.click_to_open"));
            }
            Event::MirrorEnded { key, error } => {
                let name = self.device_name(key).unwrap_or(t("common.the_device")).to_owned();
                match error {
                    Some(message) if !self.cancelled => {
                        self.notify(Kind::Error, &tf("error.mirror", &[("name", &name)]), message);
                    }
                    None if matches!(self.mirror, Mirror::Active { .. } | Mirror::Stopping { .. }) && !self.cancelled => {
                        self.notify(Kind::Connection, &tf("notify.send.stopped", &[("name", &name)]), "");
                    }
                    _ => {}
                }
            }
            Event::ReceiverStopped { error } => {
                self.extras.announced.clear();
                if let Some(message) = error {
                    self.notify(Kind::Error, t("error.receiver_stopped"), message);
                }
            }
            Event::Receiver(event) => self.notify_receiver(event),
            _ => {}
        }
    }

    fn notify_receiver(&mut self, event: &RxEvent) {
        let Some(peer) = self.clients.last().map(|c| c.peer) else { return };
        match event {
            RxEvent::MirrorStarted { .. } | RxEvent::AudioStarted { .. } if !self.extras.announced.contains(&peer) => {
                self.extras.announced.insert(peer);
                let name = self.client_name(peer);
                let key = if matches!(event, RxEvent::MirrorStarted { .. }) { "notify.receive.mirroring" } else { "notify.receive.audio" };
                self.notify(Kind::Connection, &tf(key, &[("name", &name)]), t("notify.click_to_open"));
            }
            RxEvent::Disconnected { peer } if self.extras.announced.remove(peer) => {
                let name = self.client_name(*peer);
                self.notify(Kind::Connection, &tf("notify.receive.disconnected", &[("name", &name)]), "");
            }
            _ => {}
        }
    }

    // ---------------------------------------------------------------- updates

    pub(super) fn update_available(&self) -> Option<&Release> {
        match &self.extras.update {
            UpdateState::Available(release) => Some(release),
            _ => None,
        }
    }

    pub(super) fn can_check_updates(&self) -> bool {
        crate::update::configured_repo().is_some()
    }

    pub(super) fn check_updates(&mut self) {
        if self.extras.pending.is_some() {
            return;
        }
        // While links.rs still has the placeholder there is no repository to ask.
        let Some(rx) = crate::update::spawn(self.hwnd, crate::backend::WM_BACKEND) else { return };
        self.extras.pending = Some(rx);
        self.extras.update = UpdateState::Checking;
        self.extras.next_check = Some(Instant::now() + CHECK_EVERY);
    }

    // Called from the 30-second timer; checks at start-up and then once a day.
    pub fn update_tick(&mut self) {
        if self.settings.update_check && self.extras.next_check.is_none_or(|due| Instant::now() >= due) {
            self.check_updates();
        }
    }

    pub(super) fn poll_updates(&mut self) {
        let Some(rx) = &self.extras.pending else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Outcome::Failed(String::new()),
        };
        self.extras.pending = None;
        self.extras.update = match outcome {
            Outcome::Latest(Some(release)) if crate::update::is_newer(&release.version, env!("CARGO_PKG_VERSION")) => {
                tracing::info!("hay una versión nueva de RustPlay: {}", release.version);
                if self.settings.update_notified != release.version {
                    self.settings.update_notified = release.version.clone();
                    self.settings.save();
                    self.notify(Kind::Update, &tf("update.available", &[("version", &release.version)]), t("update.notify_body"));
                }
                UpdateState::Available(release)
            }
            Outcome::Latest(_) => UpdateState::UpToDate,
            Outcome::Failed(_) => {
                self.extras.next_check = Some(Instant::now() + RETRY_AFTER);
                UpdateState::Failed
            }
        };
    }

    pub(super) fn update_status(&self) -> String {
        if let Install::Downloading(download, _) = &self.extras.install {
            return match download.progress.fraction() {
                Some(f) => tf("update.downloading_pct", &[("percent", &((f * 100.0) as u32))]),
                None => t("update.downloading_plain").to_owned(),
            };
        }
        match &self.extras.update {
            UpdateState::Idle => tf("update.current", &[("version", &env!("CARGO_PKG_VERSION"))]),
            UpdateState::Checking => t("update.checking").to_owned(),
            UpdateState::UpToDate => tf("update.up_to_date", &[("version", &env!("CARGO_PKG_VERSION"))]),
            UpdateState::Available(release) => tf("update.available", &[("version", &release.version)]),
            UpdateState::Failed => t("update.failed").to_owned(),
        }
    }

    // Design preview: 0 none, 1 update available, 2 downloading, 3 downloaded and verified.
    pub fn preview_update(&mut self, state: u8) {
        use std::sync::atomic::Ordering;
        let asset = crate::update::Asset {
            name: String::from("RustPlay-Setup-9.9.9.exe"),
            url: String::from("https://github.com/x/y/releases/download/v9.9.9/RustPlay-Setup-9.9.9.exe"),
            size: 48_000_000,
        };
        self.extras.update = match state {
            0 => UpdateState::Idle,
            _ => UpdateState::Available(Release { version: String::from("9.9.9"), url: String::new(), assets: vec![asset] }),
        };
        self.extras.install = match state {
            2 => {
                let (_tx, rx) = mpsc::channel();
                let progress = std::sync::Arc::new(updater::Progress::default());
                progress.total.store(48_000_000, Ordering::Relaxed);
                progress.done.store(17_700_000, Ordering::Relaxed);
                Install::Downloading(Download { progress, rx }, InstallerKind::Exe)
            }
            3 => Install::Ready(std::path::PathBuf::new(), InstallerKind::Exe),
            _ => Install::Idle,
        };
    }

    // The classic installer can replace itself; MSIX and portable installs only link to the page.
    pub(super) fn can_install(&self) -> bool {
        updater::can_self_update()
            && self.update_available().is_some_and(|r| updater::pick_installer(&r.assets).is_some())
    }

    fn start_install(&mut self) {
        if !matches!(self.extras.install, Install::Idle | Install::Failed(_)) || !self.can_install() {
            return;
        }
        let Some(release) = self.update_available().cloned() else { return };
        match updater::spawn(&release.assets, &release.version, self.hwnd, crate::backend::WM_BACKEND) {
            Some((download, kind)) => self.extras.install = Install::Downloading(download, kind),
            None => self.extras.install = Install::Failed("update.err.download"),
        }
    }

    pub(super) fn poll_install(&mut self) {
        let Install::Downloading(download, kind) = &self.extras.install else { return };
        let kind = *kind;
        let outcome = match download.rx.try_recv() {
            Ok(outcome) => outcome,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => updater::Outcome::Failed(String::new()),
        };
        self.extras.install = match outcome {
            updater::Outcome::Ready(path) => {
                self.ask_confirm(super::keyboard::ConfirmAction::RunInstaller);
                self.attention = true;
                Install::Ready(path, kind)
            }
            updater::Outcome::NoChecksum => Install::Failed("update.err.no_checksum"),
            updater::Outcome::Mismatch => Install::Failed("update.err.mismatch"),
            updater::Outcome::Cancelled => Install::Idle,
            updater::Outcome::Failed(e) => {
                tracing::warn!("descarga de la actualización fallida: {e}");
                Install::Failed("update.err.download")
            }
        };
    }

    // Starts the verified installer after the user's confirmation and exits RustPlay.
    pub(super) fn run_installer(&mut self) {
        let Install::Ready(path, kind) = &self.extras.install else { return };
        if updater::launch(path, *kind, self.settings.update_silent) {
            tracing::info!("instalador iniciado; RustPlay se cierra para actualizarse");
            // SAFETY: posting to the own window.
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(self.hwnd),
                    updater::WM_QUIT_FOR_UPDATE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        } else {
            self.extras.install = Install::Failed("update.err.launch");
        }
    }

    pub(super) fn extras_click(&mut self, id: Id) {
        match id {
            Id::Notifications => self.settings.notifications = !self.settings.notifications,
            Id::NotifyConnections if self.settings.notifications => {
                self.settings.notify_connections = !self.settings.notify_connections;
            }
            Id::NotifyErrors if self.settings.notifications => self.settings.notify_errors = !self.settings.notify_errors,
            Id::UpdateAuto => {
                self.settings.update_check = !self.settings.update_check;
                if self.settings.update_check && self.extras.update == UpdateState::Idle {
                    self.check_updates();
                }
            }
            Id::UpdateCheck => {
                self.check_updates();
                return;
            }
            Id::UpdateInstall => {
                self.start_install();
                return;
            }
            Id::UpdateInstallNow => {
                if matches!(self.extras.install, Install::Ready(..)) {
                    self.ask_confirm(super::keyboard::ConfirmAction::RunInstaller);
                }
                return;
            }
            Id::UpdateCancel => {
                if let Install::Downloading(download, _) = &self.extras.install {
                    download.progress.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                    self.extras.install = Install::Idle;
                }
                return;
            }
            Id::UpdateSilent => self.settings.update_silent = !self.settings.update_silent,
            Id::UpdateOpen => {
                if let Some(release) = self.update_available() {
                    crate::links::open(&release.url);
                }
                return;
            }
            _ => return,
        }
        self.settings.save();
    }
}

// ---------------------------------------------------------------- drawing

mod glyph {
    pub const BELL: char = '\u{EA8F}';
    pub const UPDATE: char = '\u{E895}';
    pub const DOWNLOAD: char = '\u{E896}';
}

impl App {
    // «Notificaciones» and «Actualizaciones» sections of the settings page; returns the new y.
    pub(super) fn draw_extras_settings(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        self.section(x, y, w, t("settings.notifications"));
        y += 30.0;
        let header = Rect::new(x, y, w, ROW_H);
        let (body, _) = self.expander(
            Exp::Notifications,
            header,
            glyph::BELL,
            t("settings.notifications"),
            t("settings.notifications_desc"),
            "",
            2,
            160.0,
        );
        let on = self.settings.notifications;
        self.toggle_with_label(Id::Notifications, header.right() - 52.0, header.y + header.h / 2.0, on, true);
        let rows = [
            (Id::NotifyConnections, t("settings.notify_connections"), t("settings.notify_connections_desc"), self.settings.notify_connections),
            (Id::NotifyErrors, t("settings.notify_errors"), t("settings.notify_errors_desc"), self.settings.notify_errors),
        ];
        self.clipped(body, |app| {
            for (i, (id, title, description, value)) in rows.into_iter().enumerate() {
                let r = app.child_row(body, i, title, description, on);
                app.toggle_with_label(id, r.right() - 16.0, r.y + r.h / 2.0, value, on);
            }
        });
        y += ROW_H + body.h + 32.0;

        self.section(x, y, w, t("settings.updates"));
        y += 30.0;
        let available = self.can_check_updates();
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::UPDATE, t("settings.update_check"), t("settings.update_check_desc"), 160.0, available);
        self.toggle_with_label(Id::UpdateAuto, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.update_check, available);
        y += ROW_H + GAP;
        let status = self.update_status();
        let checking = self.extras.update == UpdateState::Checking || matches!(self.extras.install, Install::Downloading(..));
        let label = t("update.check_now");
        let bw = self.button_width(label, None);
        let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::UPDATE, t("settings.update_status"), &status, bw + 16.0, true);
        if checking {
            let color = self.theme.accent;
            self.spinner(rect.right() - 16.0 - bw - 24.0, rect.y + rect.h / 2.0, 8.0, color);
        }
        self.button(Id::UpdateCheck, Rect::new(rect.right() - 16.0 - bw, rect.y + 18.0, bw, 32.0), label, None, Style::Standard, available && !checking);
        y += ROW_H + GAP;
        if updater::can_self_update() {
            let rect = self.row(Rect::new(x, y, w, ROW_H), glyph::UPDATE, t("settings.update_silent"), t("settings.update_silent_desc"), 160.0, available);
            self.toggle_with_label(Id::UpdateSilent, rect.right() - 16.0, rect.y + rect.h / 2.0, self.settings.update_silent, available);
            y += ROW_H + GAP;
        }
        y + 28.0
    }

    // «Hay una versión nueva» bar at the top of the about page, when there is one: view the release,
    // or download and install it (classic installer only), with progress and a cancel button.
    pub(super) fn draw_update_banner(&mut self, x: f32, y: f32, w: f32) -> f32 {
        let Some(release) = self.update_available().cloned() else { return y };
        let th = self.theme;
        let can_install = self.can_install();
        let downloading = matches!(self.extras.install, Install::Downloading(..));
        let rect = Rect::new(x, y, w, if downloading { 88.0 } else { 64.0 });
        self.card(rect);
        self.gfx.fill(rect.inset(1.0, 1.0), 7.0, th.accent.alpha(0.10));
        let cy = rect.y + rect.h / 2.0;
        self.gfx.glyph(glyph::UPDATE, Font::Icon20, rect.x + 30.0, cy, th.accent);
        let tx = rect.x + 60.0;
        let view_label = t("update.open");
        let view_w = self.button_width(view_label, Some(super::glyph::OPEN_EXTERNAL));
        let mut right = rect.right() - 16.0;
        let installed = tf("update.installed", &[("version", &env!("CARGO_PKG_VERSION"))]);
        let (title, caption, caption_color) = match &self.extras.install {
            Install::Downloading(download, _) => {
                let done = download.progress.done.load(std::sync::atomic::Ordering::Relaxed);
                let total = download.progress.total.load(std::sync::atomic::Ordering::Relaxed);
                let caption = match download.progress.fraction() {
                    Some(f) => tf("update.progress", &[("done", &megabytes(done)), ("total", &megabytes(total)), ("percent", &((f * 100.0) as u32))]),
                    None => megabytes(done),
                };
                (tf("update.downloading", &[("version", &release.version)]), caption, th.text_secondary)
            }
            Install::Ready(..) => (t("update.ready").to_owned(), t("update.ready_desc").to_owned(), th.text_secondary),
            Install::Failed(key) => (tf("update.available", &[("version", &release.version)]), t(key).to_owned(), th.critical),
            Install::Idle if !can_install => {
                let reason = if crate::autostart::is_portable() { t("update.manual_portable") } else { t("update.manual") };
                (tf("update.available", &[("version", &release.version)]), format!("{installed}  ·  {reason}"), th.text_secondary)
            }
            Install::Idle => (tf("update.available", &[("version", &release.version)]), installed, th.text_secondary),
        };
        match &self.extras.install {
            Install::Downloading(..) => {
                let label = t("common.cancel");
                let bw = self.button_width(label, None);
                self.button(Id::UpdateCancel, Rect::new(right - bw, cy - 16.0, bw, 32.0), label, None, Style::Standard, true);
                right -= bw + 8.0;
            }
            Install::Ready(..) => {
                let label = t("update.install_now");
                let bw = self.button_width(label, None);
                self.button(Id::UpdateInstallNow, Rect::new(right - bw, cy - 16.0, bw, 32.0), label, None, Style::Accent, true);
                right -= bw + 8.0;
            }
            _ if can_install => {
                let label = t("update.install");
                let bw = self.button_width(label, Some(glyph::DOWNLOAD));
                self.button(Id::UpdateInstall, Rect::new(right - bw, cy - 16.0, bw, 32.0), label, Some(glyph::DOWNLOAD), Style::Accent, true);
                right -= bw + 8.0;
                self.button(Id::UpdateOpen, Rect::new(right - view_w, cy - 16.0, view_w, 32.0), view_label, Some(super::glyph::OPEN_EXTERNAL), Style::Standard, true);
                right -= view_w + 8.0;
            }
            _ => {
                self.button(Id::UpdateOpen, Rect::new(right - view_w, cy - 16.0, view_w, 32.0), view_label, Some(super::glyph::OPEN_EXTERNAL), Style::Accent, true);
                right -= view_w + 8.0;
            }
        }
        let tw = (right - tx - 16.0).max(80.0);
        if downloading {
            self.gfx.text(&title, Font::BodyStrong, Rect::new(tx, rect.y + 14.0, tw, 20.0), th.text, Align::Start);
            self.gfx.text(&caption, Font::Caption, Rect::new(tx, rect.y + 36.0, tw, 18.0), caption_color, Align::Start);
            self.progress_bar(Rect::new(tx, rect.bottom() - 22.0, tw, 4.0));
        } else {
            self.gfx.text(&title, Font::BodyStrong, Rect::new(tx, cy - 20.0, tw, 20.0), th.text, Align::Start);
            self.gfx.text(&caption, Font::Caption, Rect::new(tx, cy + 1.0, tw, 18.0), caption_color, Align::Start);
        }
        y + rect.h + 24.0
    }

    // Determinate bar for a known size, a sliding block otherwise.
    fn progress_bar(&mut self, rect: Rect) {
        let th = self.theme;
        self.gfx.fill(rect, 2.0, th.control_strong.alpha(0.35));
        let fraction = match &self.extras.install {
            Install::Downloading(download, _) => download.progress.fraction(),
            _ => None,
        };
        self.animating = true;
        match fraction {
            Some(f) => self.gfx.fill(Rect::new(rect.x, rect.y, (rect.w * f).max(4.0), rect.h), 2.0, th.accent),
            None => {
                let phase = (self.started.elapsed().as_secs_f32() * 0.8) % 1.0;
                let block = rect.w * 0.3;
                let x = rect.x + (rect.w + block) * phase - block;
                self.clipped(rect, |app| app.gfx.fill(Rect::new(x, rect.y, block, rect.h), 2.0, th.accent));
            }
        }
    }
}

fn megabytes(bytes: u64) -> String {
    let text = format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0));
    if crate::i18n::code() == "es" { text.replace('.', ",") } else { text }
}

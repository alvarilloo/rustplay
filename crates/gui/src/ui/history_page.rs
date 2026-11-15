// The "Historial" page and the hooks that feed the history store from backend and receiver events.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Instant;

use rustplay_rx::RxEvent;

use super::widgets::Style;
use super::{App, GAP, Id, Mirror, glyph};
use crate::backend::Event;
use crate::gfx::{Align, Font, Rect};
use crate::history::{self, Entry, Kind, Store};
use crate::i18n::{self, t, tf};

const ROW_H: f32 = 64.0;

#[derive(Default)]
struct Peer {
    name: String,
    connections: u32,
    // Waiting for a name before the connection is written down.
    pending: bool,
    mirror: Option<Instant>,
    audio: Option<Instant>,
    video: Option<Instant>,
}

pub(super) struct HistoryUi {
    store: Store,
    peers: HashMap<IpAddr, Peer>,
    // Sending session in progress: when it began and whether only a window was captured.
    sending: Option<(Instant, bool)>,
    rec_start: Option<Instant>,
    rec_secs: Option<u64>,
}

impl HistoryUi {
    pub fn new(store: Store) -> Self {
        Self { store, peers: HashMap::new(), sending: None, rec_start: None, rec_secs: None }
    }
}

fn secs_since(since: Instant) -> u64 {
    since.elapsed().as_secs()
}

pub fn duration_text(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs} s"),
        60..3600 => format!("{} min", secs / 60),
        _ => match secs / 60 % 60 {
            0 => format!("{} h", secs / 3600),
            m => format!("{} h {m} min", secs / 3600),
        },
    }
}

pub fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    let number = if unit == 0 { format!("{bytes}") } else { format!("{value:.1}") };
    let number = if i18n::code() == "es" { number.replace('.', ",") } else { number };
    format!("{number} {}", UNITS[unit])
}

fn day_text(day: i64, today: i64) -> String {
    match today - day {
        0 => t("history.today").to_owned(),
        1 => t("history.yesterday").to_owned(),
        _ => {
            let (y, m, d) = history::civil(day);
            if i18n::code() == "es" { format!("{d:02}/{m:02}/{y}") } else { format!("{m:02}/{d:02}/{y}") }
        }
    }
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map_or_else(|| path.to_owned(), |n| n.to_string_lossy().into_owned())
}

// Icon, title and detail line of an entry. The address honours "hide IPs"; the file keeps it.
fn describe(entry: &Entry) -> (char, String, String) {
    let name = if entry.name.is_empty() { t("history.unknown_device").to_owned() } else { entry.name.clone() };
    let address = crate::privacy::redact(&entry.address).into_owned();
    let join = |parts: &[String]| parts.iter().filter(|p| !p.is_empty()).cloned().collect::<Vec<_>>().join("  ·  ");
    let length = if entry.secs > 0 { tf("history.duration", &[("time", &duration_text(entry.secs))]) } else { String::new() };
    let n = &name;
    match entry.kind {
        Kind::Connected => (glyph::PHONE, tf("history.connected", &[("name", n)]), address),
        Kind::MirrorStarted => (glyph::SCREEN, tf("history.mirror_started", &[("name", n)]), join(&[entry.message.clone(), address])),
        Kind::MirrorStopped => (glyph::SCREEN, tf("history.mirror_stopped", &[("name", n)]), join(&[length, address])),
        Kind::AudioStarted => (glyph::VOLUME, tf("history.audio_started", &[("name", n)]), address),
        Kind::AudioStopped => (glyph::VOLUME, tf("history.audio_stopped", &[("name", n)]), join(&[length, address])),
        Kind::VideoStarted => (glyph::VIDEO, tf("history.video_started", &[("name", n)]), address),
        Kind::VideoStopped => (glyph::VIDEO, tf("history.video_stopped", &[("name", n)]), join(&[length, address])),
        Kind::Sent => {
            let source = t(if entry.window { "history.source_window" } else { "history.source_screen" }).to_owned();
            (glyph::CAST, tf("history.sent", &[("name", n)]), join(&[length, source]))
        }
        Kind::Recording => {
            let size = if entry.size > 0 { size_text(entry.size) } else { String::new() };
            (glyph::VIDEO, t("history.recording").to_owned(), join(&[file_name(&entry.path), size, length]))
        }
        Kind::Screenshot => {
            let size = if entry.size > 0 { size_text(entry.size) } else { String::new() };
            (glyph::SCREENSHOT, t("history.screenshot").to_owned(), join(&[file_name(&entry.path), size]))
        }
        Kind::Error => {
            let title = if entry.name.is_empty() { t("history.error_plain").to_owned() } else { tf("history.error", &[("name", n)]) };
            (glyph::ERROR, title, entry.message.clone())
        }
    }
}

impl App {
    fn log_history(&mut self, entry: Entry) {
        if self.settings.save_history {
            self.hist.store.append(entry);
        }
    }

    fn peer_entry(&self, kind: Kind, ip: IpAddr) -> Entry {
        let name = self.hist.peers.get(&ip).map(|p| p.name.as_str()).unwrap_or_default();
        Entry::new(kind).named(name).address(&ip.to_string())
    }

    fn flush_connection(&mut self, ip: IpAddr) {
        if self.hist.peers.get_mut(&ip).is_some_and(|p| std::mem::take(&mut p.pending)) {
            let entry = self.peer_entry(Kind::Connected, ip);
            self.log_history(entry);
        }
    }

    // Sending sessions and errors; runs before the event is applied, so the old state is still there.
    pub(super) fn history_event(&mut self, event: &Event) {
        match event {
            Event::Mirroring { .. } => {
                if self.hist.sending.is_none() {
                    self.hist.sending = Some((Instant::now(), self.settings.share_window));
                }
            }
            Event::MirrorEnded { key, error } => {
                let name = self.device_name(key).unwrap_or_default().to_owned();
                if let Some((since, window)) = self.hist.sending.take() {
                    self.log_history(Entry::new(Kind::Sent).named(&name).secs(secs_since(since)).window(window));
                }
                if let (Some(message), false) = (error, self.cancelled) {
                    self.log_history(Entry::new(Kind::Error).named(&name).message(message));
                }
            }
            Event::ReceiverStopped { error: Some(message) } => {
                let name = self.settings.receiver_name.clone();
                self.log_history(Entry::new(Kind::Error).named(&name).message(message));
            }
            Event::Receiver(event) => self.history_receiver(event),
            _ => {}
        }
    }

    fn history_receiver(&mut self, event: &RxEvent) {
        match event {
            RxEvent::Connected { peer, name } => {
                let ip = peer.ip();
                let known = self.hist.peers.contains_key(&ip);
                let p = self.hist.peers.entry(ip).or_insert_with(|| Peer { pending: true, ..Peer::default() });
                p.connections += 1;
                if let Some(name) = name {
                    p.name.clone_from(name);
                }
                if !known && name.is_some() {
                    self.flush_connection(ip);
                }
            }
            RxEvent::ClientName { peer, name } => {
                if let Some(p) = self.hist.peers.get_mut(&peer.ip()) {
                    p.name.clone_from(name);
                    self.flush_connection(peer.ip());
                }
            }
            RxEvent::AudioStarted { peer, .. } => self.peer_started(Kind::AudioStarted, peer.ip(), String::new()),
            RxEvent::MirrorStarted { peer, width, height } => self.peer_started(Kind::MirrorStarted, peer.ip(), format!("{width}x{height}")),
            RxEvent::VideoStarted { peer } => self.peer_started(Kind::VideoStarted, peer.ip(), String::new()),
            RxEvent::StreamStopped { peer } => self.peer_stopped(Kind::AudioStopped, peer.ip()),
            RxEvent::MirrorStopped { peer } => self.peer_stopped(Kind::MirrorStopped, peer.ip()),
            RxEvent::VideoStopped => {
                let ip = self.hist.peers.iter().find(|(_, p)| p.video.is_some()).map(|(ip, _)| *ip);
                if let Some(ip) = ip {
                    self.peer_stopped(Kind::VideoStopped, ip);
                }
            }
            RxEvent::Disconnected { peer } => {
                let ip = peer.ip();
                let Some(p) = self.hist.peers.get_mut(&ip) else { return };
                p.connections = p.connections.saturating_sub(1);
                if p.connections > 0 {
                    return;
                }
                for kind in [Kind::MirrorStopped, Kind::AudioStopped, Kind::VideoStopped] {
                    self.peer_stopped(kind, ip);
                }
                self.hist.peers.remove(&ip);
            }
            RxEvent::RecordingState { active } => {
                if *active {
                    self.hist.rec_start.get_or_insert_with(Instant::now);
                } else {
                    self.hist.rec_secs = self.hist.rec_start.take().map(secs_since);
                }
            }
            RxEvent::RecordingSaved { path } => {
                let secs = self.hist.rec_secs.take().or_else(|| self.hist.rec_start.map(secs_since)).unwrap_or(0);
                self.log_history(Entry::new(Kind::Recording).file(path).secs(secs));
            }
            RxEvent::ScreenshotSaved { path } => self.log_history(Entry::new(Kind::Screenshot).file(path)),
            RxEvent::RecordingFailed { message } | RxEvent::ScreenshotFailed { message } => {
                self.log_history(Entry::new(Kind::Error).message(message));
            }
            _ => {}
        }
    }

    fn peer_started(&mut self, kind: Kind, ip: IpAddr, detail: String) {
        self.flush_connection(ip);
        let Some(p) = self.hist.peers.get_mut(&ip) else { return };
        let slot = match kind {
            Kind::MirrorStarted => &mut p.mirror,
            Kind::AudioStarted => &mut p.audio,
            _ => &mut p.video,
        };
        // Mirroring resumes after a lock screen without a new start.
        if slot.replace(Instant::now()).is_some() {
            return;
        }
        let entry = self.peer_entry(kind, ip).message(&detail);
        self.log_history(entry);
    }

    // `kind` is the stop event; writes it only if the matching start was seen.
    fn peer_stopped(&mut self, kind: Kind, ip: IpAddr) {
        let Some(p) = self.hist.peers.get_mut(&ip) else { return };
        let slot = match kind {
            Kind::MirrorStopped => &mut p.mirror,
            Kind::AudioStopped => &mut p.audio,
            _ => &mut p.video,
        };
        if let Some(since) = slot.take() {
            let entry = self.peer_entry(kind, ip).secs(secs_since(since));
            self.log_history(entry);
        }
    }

    // A sending session still running when RustPlay exits.
    pub(super) fn history_shutdown(&mut self) {
        if let Mirror::Active { key, .. } = &self.mirror.clone()
            && let Some((since, window)) = self.hist.sending.take()
        {
            let name = self.device_name(key).unwrap_or_default().to_owned();
            self.log_history(Entry::new(Kind::Sent).named(&name).secs(secs_since(since)).window(window));
        }
    }

    pub(super) fn history_click(&mut self, id: Id) -> bool {
        match id {
            Id::HistoryClear => self.ask_confirm(super::keyboard::ConfirmAction::ClearHistory),
            Id::HistoryOpen(i) => {
                if let Some(e) = self.hist.store.entries().get(i) {
                    crate::folder::open_file(std::path::Path::new(&e.path));
                }
            }
            Id::HistoryReveal(i) => {
                if let Some(e) = self.hist.store.entries().get(i) {
                    crate::folder::reveal(std::path::Path::new(&e.path));
                }
            }
            Id::SaveHistory => {
                self.settings.save_history = !self.settings.save_history;
                self.settings.save();
            }
            _ => return false,
        }
        true
    }

    pub(super) fn history_clear(&mut self) {
        self.hist.store.clear();
        self.scroll_target = 0.0;
        self.toast(t("history.cleared"));
    }

    pub(super) fn draw_history(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        let count = self.hist.store.entries().len();
        let clear_label = t("history.clear");
        let clear_w = self.button_width(clear_label, Some(glyph::DELETE));
        self.gfx.text(&tf("history.count", &[("count", &count)]), Font::BodyStrong, Rect::new(x + 2.0, y + 6.0, w - clear_w - 16.0, 20.0), th.text, Align::Start);
        self.button(Id::HistoryClear, Rect::new(x + w - clear_w, y, clear_w, 32.0), clear_label, Some(glyph::DELETE), Style::Standard, count > 0);
        y += 42.0;
        if !self.settings.save_history {
            self.gfx.text(t("history.disabled"), Font::Caption, Rect::new(x + 2.0, y - 4.0, w, 18.0), th.text_secondary, Align::Start);
            y += 24.0;
        }

        if count == 0 {
            let rect = Rect::new(x, y, w, 72.0);
            self.card(rect);
            let cy = rect.y + rect.h / 2.0;
            let tile = Rect::new(rect.x + 16.0, cy - 20.0, 40.0, 40.0);
            self.gfx.fill(tile, 8.0, th.text_secondary.alpha(0.1));
            self.gfx.glyph(glyph::HISTORY, Font::Icon20, tile.x + 20.0, tile.y + 20.0, th.text_secondary);
            let tx = rect.x + 72.0;
            let tw = rect.w - 72.0 - 16.0;
            self.gfx.text(t("history.empty"), Font::Body, Rect::new(tx, cy - 20.0, tw, 20.0), th.text, Align::Start);
            self.gfx.text(t("history.empty_hint"), Font::Caption, Rect::new(tx, cy, tw, 18.0), th.text_secondary, Align::Start);
            return y + rect.h;
        }

        let offset = history::local_offset_secs();
        let today = (history::now() as i64 + offset).div_euclid(86_400);
        let view = self.clip;
        // Cloned so drawing can borrow the app mutably; at most 500 small entries.
        let entries: Vec<Entry> = self.hist.store.entries().to_vec();
        let groups = history::group_by_day(&entries, offset);
        for (g, group) in groups.iter().enumerate() {
            if g > 0 {
                y += 16.0;
            }
            self.section(x, y, w, &day_text(group.day, today));
            y += 30.0;
            for (index, entry) in &group.items {
                let rect = Rect::new(x, y, w, ROW_H);
                y += ROW_H + GAP;
                let visible = view.is_none_or(|v| rect.bottom() > v.y && rect.y < v.bottom());
                let (open, reveal) = Self::history_buttons(rect, self.button_width(t("history.open"), None), self.button_width(t("history.open_folder"), Some(glyph::FOLDER)));
                let has_file = !entry.path.is_empty();
                if !visible {
                    // Off-screen rows still take part in the Tab order.
                    if has_file {
                        self.note_focusable(open, Id::HistoryOpen(*index));
                        self.note_focusable(reveal, Id::HistoryReveal(*index));
                    }
                    continue;
                }
                self.draw_history_row(rect, *index, entry, offset, (open, reveal));
            }
        }
        y - GAP
    }

    // Rects of the "Abrir" and "Abrir carpeta" buttons of a row.
    fn history_buttons(rect: Rect, open_w: f32, reveal_w: f32) -> (Rect, Rect) {
        let cy = rect.y + rect.h / 2.0;
        let reveal = Rect::new(rect.right() - 16.0 - 56.0 - 12.0 - reveal_w, cy - 16.0, reveal_w, 32.0);
        let open = Rect::new(reveal.x - 8.0 - open_w, cy - 16.0, open_w, 32.0);
        (open, reveal)
    }

    fn draw_history_row(&mut self, rect: Rect, index: usize, entry: &Entry, offset: i64, (open, reveal): (Rect, Rect)) {
        let th = self.theme;
        self.card(rect);
        let (icon, title, detail) = describe(entry);
        let cy = rect.y + rect.h / 2.0;
        let tile = Rect::new(rect.x + 16.0, cy - 20.0, 40.0, 40.0);
        let (tint, glyph_color) = if entry.kind == Kind::Error { (th.critical, th.critical) } else { (th.text_secondary, th.text) };
        self.gfx.fill(tile, 8.0, tint.alpha(0.12));
        self.gfx.glyph(icon, Font::Icon20, tile.x + 20.0, tile.y + 20.0, glyph_color);
        let has_file = !entry.path.is_empty();
        let right_edge = if has_file { open.x - 16.0 } else { rect.right() - 16.0 - 56.0 - 12.0 };
        let tx = rect.x + 72.0;
        let tw = (right_edge - tx).max(40.0);
        self.gfx.text(&title, Font::Body, Rect::new(tx, cy - 20.0, tw, 20.0), th.text, Align::Start);
        self.gfx.text(&detail, Font::Caption, Rect::new(tx, cy, tw, 18.0), th.text_secondary, Align::Start);
        let (h, m) = history::clock(entry.ts, offset);
        self.gfx.text(&format!("{h:02}:{m:02}"), Font::Caption, Rect::new(rect.right() - 16.0 - 56.0, cy - 10.0, 56.0, 20.0), th.text_secondary, Align::End);
        if has_file {
            let exists = std::path::Path::new(&entry.path).is_file();
            self.button(Id::HistoryOpen(index), open, t("history.open"), None, Style::Standard, exists);
            self.button(Id::HistoryReveal(index), reveal, t("history.open_folder"), Some(glyph::FOLDER), Style::Standard, true);
        }
    }

    // Preview only: some made-up entries and devices to review the design.
    pub fn preview_populate(&mut self) {
        if !self.hist.store.entries().is_empty() {
            return;
        }
        let now = history::now();
        let mut store = Store::in_memory();
        let at = |ago: u64, kind| Entry::at(kind, now.saturating_sub(ago));
        let rows = [
            at(3 * 86_400 + 600, Kind::Connected).named("iPhone de Álvaro").address("192.168.1.42"),
            at(3 * 86_400 + 500, Kind::AudioStarted).named("iPhone de Álvaro").address("192.168.1.42"),
            at(3 * 86_400 + 100, Kind::AudioStopped).named("iPhone de Álvaro").address("192.168.1.42").secs(400),
            at(86_400 + 4000, Kind::Sent).named("Salón").secs(3 * 3600 + 15 * 60),
            at(86_400 + 900, Kind::Sent).named("Dormitorio").secs(1800).window(true),
            at(7200, Kind::Connected).named("iPad").address("192.168.1.57"),
            at(7100, Kind::MirrorStarted).named("iPad").address("192.168.1.57").message("2388x1668"),
            Entry { path: "C:\\Users\\Álvaro\\Videos\\RustPlay\\RustPlay 2026-11-15 10-20-31.mp4".into(), size: 48_500_000, ..at(4000, Kind::Recording).secs(312) },
            Entry { path: "C:\\Users\\Álvaro\\Pictures\\RustPlay\\Captura 2026-11-15 10-22-02.png".into(), size: 1_340_000, ..at(3900, Kind::Screenshot) },
            at(3000, Kind::MirrorStopped).named("iPad").address("192.168.1.57").secs(4100),
            at(1200, Kind::Error).named("Salón").message("Se perdió la conexión con el dispositivo"),
            at(300, Kind::VideoStarted).named("iPhone de Álvaro").address("192.168.1.42"),
        ];
        for entry in rows {
            store.append(entry);
        }
        self.hist = HistoryUi::new(store);
        self.settings.save_history = true;
    }
}

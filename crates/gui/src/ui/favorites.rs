// Favourite devices and their custom names: the device cards of the Send page, the inline
// name editor and the helpers the tray menu uses.

use std::time::Instant;

use super::widgets::Style;
use super::{App, Id, Mirror, glyph};
use crate::gfx::{Align, Color, Font, Rect};
use crate::i18n::t;
use crate::settings::{ALIAS_MAX_CHARS, Favorite};

const CARD_H: f32 = 72.0;
const ICON_BUTTON: f32 = 32.0;
// Room kept at the right of a card for the star and the pencil.
const TOOLS_W: f32 = 2.0 * ICON_BUTTON + 4.0 + 12.0;

pub(super) struct AliasEdit {
    pub key: String,
    pub text: String,
}

#[derive(Clone, Copy)]
enum Row {
    // Index into `devices`.
    Found(usize),
    // Index into the saved favourites; the device is not on the network now.
    Offline(usize),
}

impl App {
    // Favourites that are on the network, as (device index, name to show), for the tray menu.
    pub(super) fn favorite_devices(&self) -> Vec<(usize, String)> {
        let mut found: Vec<(usize, String)> = self
            .devices
            .iter()
            .enumerate()
            .filter(|(_, d)| self.settings.is_favorite(&d.key))
            .map(|(i, d)| (i, self.shown_name(&d.key, &d.name)))
            .collect();
        found.sort_by_key(|(_, name)| name.to_lowercase());
        found
    }

    // The custom name if there is one, else the name on the network.
    pub(super) fn shown_name(&self, key: &str, real: &str) -> String {
        self.settings.alias(key).unwrap_or(real).to_owned()
    }

    // Keeps the saved name and kind of the favourites that are on the network up to date.
    pub(super) fn refresh_favorites(&mut self) {
        let mut changed = false;
        for favorite in &mut self.settings.favorites {
            if let Some(d) = self.devices.iter().find(|d| d.key == favorite.key)
                && (favorite.name != d.name || favorite.speaker != d.speaker)
            {
                favorite.name.clone_from(&d.name);
                favorite.speaker = d.speaker;
                changed = true;
            }
        }
        if changed {
            self.settings.save();
        }
    }

    fn rows(&self) -> (Vec<Row>, Vec<Row>) {
        let mut favorites: Vec<(String, bool, Row)> = Vec::new();
        let mut others: Vec<(String, Row)> = Vec::new();
        for (i, d) in self.devices.iter().enumerate() {
            let name = self.shown_name(&d.key, &d.name).to_lowercase();
            if self.settings.is_favorite(&d.key) {
                favorites.push((name, false, Row::Found(i)));
            } else {
                others.push((name, Row::Found(i)));
            }
        }
        for (i, f) in self.settings.favorites.iter().enumerate() {
            if !self.devices.iter().any(|d| d.key == f.key) {
                favorites.push((self.shown_name(&f.key, &f.name).to_lowercase(), true, Row::Offline(i)));
            }
        }
        favorites.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
        others.sort_by(|a, b| a.0.cmp(&b.0));
        (favorites.into_iter().map(|(_, _, r)| r).collect(), others.into_iter().map(|(_, r)| r).collect())
    }

    // Device cards: the favourites group first (also the ones that are not found now), then the rest.
    pub(super) fn draw_device_cards(&mut self, x: f32, mut y: f32, w: f32) -> f32 {
        let th = self.theme;
        let (favorites, others) = self.rows();
        let grouped = !favorites.is_empty();
        if grouped {
            self.gfx.text(t("send.favorites.title"), Font::Caption, Rect::new(x + 2.0, y, w, 18.0), th.text_secondary, Align::Start);
            y += 24.0;
        }
        for row in &favorites {
            y = self.device_card(x, y, w, *row);
        }
        if grouped && !others.is_empty() {
            y += 16.0;
            self.gfx.text(t("send.favorites.others"), Font::Caption, Rect::new(x + 2.0, y, w, 18.0), th.text_secondary, Align::Start);
            y += 24.0;
        }
        for row in &others {
            y = self.device_card(x, y, w, *row);
        }
        y
    }

    fn device_card(&mut self, x: f32, y: f32, w: f32, row: Row) -> f32 {
        let th = self.theme;
        let idle = self.mirror == Mirror::Idle;
        // (key, name on the network, detail line, speaker, device index when found)
        let (key, real, base_detail, speaker, found) = match row {
            Row::Found(i) => {
                let d = &self.devices[i];
                (d.key.clone(), d.name.clone(), d.detail(), d.speaker, Some(i))
            }
            Row::Offline(i) => {
                let f = &self.settings.favorites[i];
                (f.key.clone(), f.name.clone(), String::new(), f.speaker, None)
            }
        };
        let alias = self.settings.alias(&key).map(str::to_owned);
        let title = alias.clone().unwrap_or_else(|| real.clone());
        let detail = match (&alias, found) {
            (Some(_), Some(_)) => [real.as_str(), base_detail.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join("  ·  "),
            (Some(_), None) => real.clone(),
            (None, _) => base_detail,
        };
        let rect = Rect::new(x, y, w, CARD_H);
        let mine = found.is_some() && self.mirror.key() == Some(key.as_str());
        let busy = found.is_some() && !idle && !mine;
        let unavailable = found.is_none();
        match found {
            Some(i) => {
                self.card_interactive(Id::DeviceCard(i), rect, idle);
            }
            None => self.card(rect),
        }
        let cy = rect.y + rect.h / 2.0;
        let tile = Rect::new(rect.x + 16.0, cy - 20.0, 40.0, 40.0);
        let tint = if mine { th.accent } else { th.text_secondary };
        self.gfx.fill(tile, 8.0, tint.alpha(if mine { 0.18 } else { 0.1 }));
        let icon = if speaker { glyph::SPEAKER } else { glyph::TV };
        let dim = busy || unavailable;
        self.gfx.glyph(icon, Font::Icon20, tile.x + 20.0, tile.y + 20.0, if dim { th.text_disabled } else { th.text });
        let (primary, secondary) = if dim { (th.text_disabled, th.text_disabled) } else { (th.text, th.text_secondary) };
        let tx = rect.x + 72.0;
        let right = rect.right() - 16.0;

        if self.alias_edit.as_ref().is_some_and(|e| e.key == key) {
            self.draw_alias_editor(rect, tx);
            return y + rect.h + super::GAP;
        }

        let tw = rect.w - 72.0 - 320.0 - TOOLS_W;
        self.gfx.text(&title, Font::Body, Rect::new(tx, cy - 20.0, tw, 20.0), primary, Align::Start);
        self.gfx.text(&detail, Font::Caption, Rect::new(tx, cy, tw, 18.0), secondary, Align::Start);

        // Star and pencil, always at the far right.
        let star_rect = Rect::new(right - ICON_BUTTON, cy - ICON_BUTTON / 2.0, ICON_BUTTON, ICON_BUTTON);
        let pencil_rect = Rect::new(star_rect.x - 4.0 - ICON_BUTTON, star_rect.y, ICON_BUTTON, ICON_BUTTON);
        let favorite = self.settings.is_favorite(&key);
        let (star_id, edit_id) = match row {
            Row::Found(i) => (Id::DeviceFav(i), Id::DeviceEdit(i)),
            Row::Offline(i) => (Id::FavOffline(i), Id::FavOfflineEdit(i)),
        };
        let star_color = if favorite { th.accent } else { th.text_secondary };
        self.icon_button(star_id, star_rect, if favorite { glyph::STAR_FILL } else { glyph::STAR }, star_color);
        self.icon_button(edit_id, pencil_rect, glyph::EDIT, th.text_secondary);

        let right = right - TOOLS_W + 4.0;
        if unavailable {
            let label = t("send.favorites.unavailable");
            let (lw, _) = self.gfx.measure(label, Font::Body, 400.0, false);
            self.gfx.text(label, Font::Body, Rect::new(right - lw - 2.0, cy - 10.0, lw + 4.0, 20.0), th.text_disabled, Align::Start);
            return y + rect.h + super::GAP;
        }
        let index = found.unwrap_or_default();
        match &self.mirror {
            Mirror::Active { .. } if mine => {
                let bw = self.button_width(t("common.stop"), Some(glyph::STOP));
                let badge_w = self.badge_width(t("common.live"));
                self.badge(right - bw - 12.0 - badge_w, cy, t("common.live"), th.success);
                self.button(Id::Device(index), Rect::new(right - bw, cy - 16.0, bw, 32.0), t("common.stop"), Some(glyph::STOP), Style::Standard, true);
            }
            Mirror::Connecting { .. } | Mirror::Reconnecting { .. } if mine => {
                let bw = self.button_width(t("common.cancel"), None);
                let status = if matches!(self.mirror, Mirror::Reconnecting { .. }) { t("status.reconnecting_short") } else { t("status.connecting") };
                self.spinner(right - bw - 126.0, cy, 8.0, th.accent);
                self.gfx.text(status, Font::Body, Rect::new(right - bw - 110.0, cy - 10.0, 100.0, 20.0), th.text_secondary, Align::Start);
                self.button(Id::Device(index), Rect::new(right - bw, cy - 16.0, bw, 32.0), t("common.cancel"), None, Style::Standard, true);
            }
            Mirror::Stopping { .. } if mine => {
                self.spinner(right - 106.0, cy, 8.0, th.text_secondary);
                self.gfx.text(t("common.stopping"), Font::Body, Rect::new(right - 90.0, cy - 10.0, 90.0, 20.0), th.text_secondary, Align::Start);
            }
            _ => {
                let bw = self.button_width(t("send.devices.mirror"), Some(glyph::CAST));
                self.button(Id::Device(index), Rect::new(right - bw, cy - 16.0, bw, 32.0), t("send.devices.mirror"), Some(glyph::CAST), Style::Accent, idle);
            }
        }
        y + rect.h + super::GAP
    }

    // A square subtle button with only a glyph, like the ones in WinUI command bars.
    pub(super) fn icon_button(&mut self, id: Id, rect: Rect, icon: char, color: Color) {
        let (hover, pressed) = self.interact(id, rect, true);
        let th = self.theme;
        let fill = if pressed { th.subtle_pressed } else { th.subtle_hover.alpha(hover) };
        self.gfx.fill(rect, 4.0, fill);
        self.hc_hover(rect, 4.0, hover);
        let color = if pressed { color.alpha(0.7) } else { color };
        self.gfx.glyph(icon, Font::Icon, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, color);
    }

    fn draw_alias_editor(&mut self, rect: Rect, tx: f32) {
        let cy = rect.y + rect.h / 2.0;
        let right = rect.right() - 16.0;
        let cancel_w = self.button_width(t("common.cancel"), None);
        let save_w = self.button_width(t("common.save"), None);
        let cancel = Rect::new(right - cancel_w, cy - 16.0, cancel_w, 32.0);
        let save = Rect::new(cancel.x - 8.0 - save_w, cy - 16.0, save_w, 32.0);
        let text = self.alias_edit.as_ref().map(|e| e.text.clone()).unwrap_or_default();
        self.text_box(Id::AliasBox, Rect::new(tx, cy - 16.0, save.x - 12.0 - tx, 32.0), &text, true);
        self.button(Id::AliasOk, save, t("common.save"), None, Style::Accent, true);
        self.button(Id::AliasCancel, cancel, t("common.cancel"), None, Style::Standard, true);
    }

    pub(super) fn favorites_click(&mut self, id: Id) -> bool {
        match id {
            Id::DeviceFav(i) => {
                if let Some(d) = self.devices.get(i) {
                    let (key, name, speaker) = (d.key.clone(), d.name.clone(), d.speaker);
                    self.toggle_favorite(key, name, speaker);
                }
            }
            Id::FavOffline(i) => {
                if let Some(f) = self.settings.favorites.get(i).cloned() {
                    self.toggle_favorite(f.key, f.name, f.speaker);
                }
            }
            Id::DeviceEdit(i) => {
                if let Some(key) = self.devices.get(i).map(|d| d.key.clone()) {
                    self.start_alias(key);
                }
            }
            Id::FavOfflineEdit(i) => {
                if let Some(key) = self.settings.favorites.get(i).map(|f| f.key.clone()) {
                    self.start_alias(key);
                }
            }
            Id::AliasOk => self.commit_alias(),
            Id::AliasCancel => self.cancel_alias(),
            _ => return false,
        }
        true
    }

    fn toggle_favorite(&mut self, key: String, name: String, speaker: bool) {
        if self.settings.is_favorite(&key) {
            self.settings.favorites.retain(|f| f.key != key);
        } else {
            self.settings.favorites.push(Favorite { key, name, speaker });
        }
        self.settings.save();
    }

    fn start_alias(&mut self, key: String) {
        let text = self.settings.alias(&key).unwrap_or_default().to_owned();
        self.alias_edit = Some(AliasEdit { key, text });
        self.focus = Some(Id::AliasBox);
        self.kb_focus = Some(Id::AliasBox);
        self.caret_epoch = Instant::now();
    }

    pub(super) fn commit_alias(&mut self) {
        if let Some(edit) = self.alias_edit.take() {
            self.settings.set_alias(&edit.key, &edit.text);
            self.settings.save();
        }
        self.end_alias_focus();
    }

    pub(super) fn cancel_alias(&mut self) {
        self.alias_edit = None;
        self.end_alias_focus();
    }

    fn end_alias_focus(&mut self) {
        if self.focus == Some(Id::AliasBox) {
            self.focus = None;
        }
    }

    pub(super) fn alias_char(&mut self, c: char) {
        let Some(edit) = self.alias_edit.as_mut() else { return };
        match c {
            '\u{8}' => {
                edit.text.pop();
            }
            '\r' => self.commit_alias(),
            '\u{1b}' => self.cancel_alias(),
            '\t' => {}
            c if !c.is_control() && edit.text.chars().count() < ALIAS_MAX_CHARS => edit.text.push(c),
            _ => {}
        }
    }

    pub(super) fn alias_paste(&mut self, text: &str) {
        let Some(edit) = self.alias_edit.as_mut() else { return };
        for c in text.chars().filter(|c| !c.is_control()) {
            if edit.text.chars().count() < ALIAS_MAX_CHARS {
                edit.text.push(c);
            }
        }
    }

    pub fn preview_alias(&mut self, on: bool) {
        self.alias_edit = None;
        self.focus = None;
        if on && let Some(f) = self.settings.favorites.first().map(|f| f.key.clone()) {
            self.start_alias(f);
            if let Some(edit) = self.alias_edit.as_mut() {
                edit.text = String::from("Salón de abajo");
            }
        }
    }

    // Preview only: made-up devices, favourites and aliases to review the design.
    pub fn preview_devices(&mut self) {
        if !self.devices.is_empty() {
            return;
        }
        let make = |instance: &str, model: &str, address: &str, id: &str| {
            let device = rustplay_core::mdns::DiscoveredDevice {
                instance: instance.to_owned(),
                service_type: String::from("_airplay._tcp"),
                host: String::new(),
                addrs: vec![address.parse().unwrap()],
                port: 7000,
                txt: [("deviceid".to_owned(), id.to_owned()), ("model".to_owned(), model.to_owned())].into_iter().collect(),
            };
            super::DeviceEntry {
                key: id.to_owned(),
                name: instance.to_owned(),
                speaker: model.starts_with("AudioAccessory"),
                model: super::friendly_model(model),
                address: address.to_owned(),
                device,
            }
        };
        self.devices = vec![
            make("Apple TV Salón", "AppleTV6,2", "192.168.1.20", "AA:BB:CC:00:00:01"),
            make("Dormitorio", "AppleTV5,3", "192.168.1.21", "AA:BB:CC:00:00:02"),
            make("HomePod cocina", "AudioAccessory5,1", "192.168.1.22", "AA:BB:CC:00:00:03"),
            make("Samsung TV", "UN55", "192.168.1.30", "AA:BB:CC:00:00:04"),
        ];
        self.searched = true;
        self.settings.favorites = vec![
            Favorite { key: "AA:BB:CC:00:00:01".into(), name: "Apple TV Salón".into(), speaker: false },
            Favorite { key: "AA:BB:CC:00:00:09".into(), name: "Apple TV Despacho".into(), speaker: false },
        ];
        self.settings.set_alias("AA:BB:CC:00:00:01", "Salón");
        self.settings.set_alias("AA:BB:CC:00:00:09", "Despacho");
    }
}
